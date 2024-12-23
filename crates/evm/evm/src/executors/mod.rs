//! EVM executor abstractions, which can execute calls.
//!
//! Used for running tests, scripts, and interacting with the inner backend which holds the state.

// TODO: The individual executors in this module should be moved into the respective crates, and the
// `Executor` struct should be accessed using a trait defined in `foundry-evm-core` instead of
// the concrete `Executor` type.

use crate::inspectors::{
    cheatcodes::BroadcastableTransactions, Cheatcodes, InspectorData, InspectorStack,
};
use alloy_dyn_abi::{DynSolValue, FunctionExt, JsonAbiExt};
use alloy_json_abi::Function;
use alloy_primitives::{
    map::{AddressHashMap, HashMap},
    Address, Bytes, Log, U256,
};
use alloy_sol_types::{sol, SolCall};
use foundry_evm_core::{
    backend::{Backend, BackendError, BackendResult, CowBackend, DatabaseExt, GLOBAL_FAIL_SLOT},
    constants::{
        CALLER, CHEATCODE_ADDRESS, CHEATCODE_CONTRACT_HASH, DEFAULT_CREATE2_DEPLOYER,
        DEFAULT_CREATE2_DEPLOYER_CODE, DEFAULT_CREATE2_DEPLOYER_DEPLOYER,
    },
    decode::{RevertDecoder, SkipReason},
    utils::StateChangeset,
};
use foundry_evm_coverage::HitMaps;
use foundry_evm_traces::{SparsedTraceArena, TraceMode};
use revm::{
    db::{DatabaseCommit, DatabaseRef},
    interpreter::{return_ok, InstructionResult},
    primitives::{
        AuthorizationList, BlockEnv, Bytecode, Env, EnvWithHandlerCfg, ExecutionResult, Output,
        ResultAndState, SignedAuthorization, SpecId, TxEnv, TxKind,
    },
};
use std::borrow::Cow;
use strategy::ExecutorStrategy;

mod builder;
pub use builder::ExecutorBuilder;

pub mod fuzz;
pub use fuzz::FuzzedExecutor;

pub mod invariant;
pub use invariant::InvariantExecutor;

mod trace;
pub use trace::TracingExecutor;

pub mod strategy;

sol! {
    interface ITest {
        function setUp() external;
        function failed() external view returns (bool failed);

        #[derive(Default)]
        function beforeTestSetup(bytes4 testSelector) public view returns (bytes[] memory beforeTestCalldata);
    }
}

/// EVM executor.
///
/// The executor can be configured with various `revm::Inspector`s, like `Cheatcodes`.
///
/// There are multiple ways of interacting the EVM:
/// - `call`: executes a transaction, but does not persist any state changes; similar to `eth_call`,
///   where the EVM state is unchanged after the call.
/// - `transact`: executes a transaction and persists the state changes
/// - `deploy`: a special case of `transact`, specialized for persisting the state of a contract
///   deployment
/// - `setup`: a special case of `transact`, used to set up the environment for a test
#[derive(Debug)]
pub struct Executor {
    /// The underlying `revm::Database` that contains the EVM storage.
    // Note: We do not store an EVM here, since we are really
    // only interested in the database. REVM's `EVM` is a thin
    // wrapper around spawning a new EVM on every call anyway,
    // so the performance difference should be negligible.
    pub backend: Backend,
    /// The EVM environment.
    pub env: EnvWithHandlerCfg,
    /// The Revm inspector stack.
    pub inspector: InspectorStack,
    /// The gas limit for calls and deployments. This is different from the gas limit imposed by
    /// the passed in environment, as those limits are used by the EVM for certain opcodes like
    /// `gaslimit`.
    gas_limit: u64,
    /// Whether `failed()` should be called on the test contract to determine if the test failed.
    legacy_assertions: bool,

    pub strategy: ExecutorStrategy,
}

impl Clone for Executor {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            env: self.env.clone(),
            inspector: self.inspector.clone(),
            gas_limit: self.gas_limit,
            legacy_assertions: self.legacy_assertions,
            strategy: self.strategy.clone(),
        }
    }
}

impl Executor {
    /// Creates a new `ExecutorBuilder`.
    #[inline]
    pub fn builder() -> ExecutorBuilder {
        ExecutorBuilder::new()
    }

    /// Creates a new `Executor` with the given arguments.
    #[inline]
    pub fn new(
        mut backend: Backend,
        env: EnvWithHandlerCfg,
        inspector: InspectorStack,
        gas_limit: u64,
        legacy_assertions: bool,
        strategy: ExecutorStrategy,
    ) -> Self {
        // Need to create a non-empty contract on the cheatcodes address so `extcodesize` checks
        // do not fail.
        backend.insert_account_info(
            CHEATCODE_ADDRESS,
            revm::primitives::AccountInfo {
                code: Some(Bytecode::new_raw(Bytes::from_static(&[0]))),
                // Also set the code hash manually so that it's not computed later.
                // The code hash value does not matter, as long as it's not zero or `KECCAK_EMPTY`.
                code_hash: CHEATCODE_CONTRACT_HASH,
                ..Default::default()
            },
        );

        Self { backend, env, inspector, gas_limit, legacy_assertions, strategy }
    }

    fn clone_with_backend(&self, backend: Backend) -> Self {
        let env = EnvWithHandlerCfg::new_with_spec_id(Box::new(self.env().clone()), self.spec_id());
        Self::new(
            backend,
            env,
            self.inspector().clone(),
            self.gas_limit,
            self.legacy_assertions,
            self.strategy.clone(),
        )
    }

    /// Returns a reference to the EVM backend.
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Returns a mutable reference to the EVM backend.
    pub fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }

    /// Returns a reference to the EVM environment.
    pub fn env(&self) -> &Env {
        &self.env.env
    }

    /// Returns a mutable reference to the EVM environment.
    pub fn env_mut(&mut self) -> &mut Env {
        &mut self.env.env
    }

    /// Returns a reference to the EVM inspector.
    pub fn inspector(&self) -> &InspectorStack {
        &self.inspector
    }

    /// Returns a mutable reference to the EVM inspector.
    pub fn inspector_mut(&mut self) -> &mut InspectorStack {
        &mut self.inspector
    }

    /// Returns the EVM spec ID.
    pub fn spec_id(&self) -> SpecId {
        self.env.spec_id()
    }

    /// Creates the default CREATE2 Contract Deployer for local tests and scripts.
    pub fn deploy_create2_deployer(&mut self) -> eyre::Result<()> {
        trace!("deploying local create2 deployer");
        let create2_deployer_account = self
            .backend()
            .basic_ref(DEFAULT_CREATE2_DEPLOYER)?
            .ok_or_else(|| BackendError::MissingAccount(DEFAULT_CREATE2_DEPLOYER))?;

        // If the deployer is not currently deployed, deploy the default one.
        if create2_deployer_account.code.map_or(true, |code| code.is_empty()) {
            let creator = DEFAULT_CREATE2_DEPLOYER_DEPLOYER;

            // Probably 0, but just in case.
            let initial_balance = self.get_balance(creator)?;
            self.set_balance(creator, U256::MAX)?;

            let res =
                self.deploy(creator, DEFAULT_CREATE2_DEPLOYER_CODE.into(), U256::ZERO, None)?;
            trace!(create2=?res.address, "deployed local create2 deployer");

            self.set_balance(creator, initial_balance)?;
        }
        Ok(())
    }

    /// Set the balance of an account.
    pub fn set_balance(&mut self, address: Address, amount: U256) -> BackendResult<()> {
        trace!(?address, ?amount, "setting account balance");
        self.strategy.runner.clone().set_balance(self, address, amount)
    }

    /// Gets the balance of an account
    pub fn get_balance(&self, address: Address) -> BackendResult<U256> {
        Ok(self.backend().basic_ref(address)?.map(|acc| acc.balance).unwrap_or_default())
    }

    /// Set the nonce of an account.
    pub fn set_nonce(&mut self, address: Address, nonce: u64) -> BackendResult<()> {
        self.strategy.runner.clone().set_nonce(self, address, nonce)
    }

    /// Returns the nonce of an account.
    pub fn get_nonce(&self, address: Address) -> BackendResult<u64> {
        Ok(self.backend().basic_ref(address)?.map(|acc| acc.nonce).unwrap_or_default())
    }

    /// Returns `true` if the account has no code.
    pub fn is_empty_code(&self, address: Address) -> BackendResult<bool> {
        Ok(self.backend().basic_ref(address)?.map(|acc| acc.is_empty_code_hash()).unwrap_or(true))
    }

    #[inline]
    pub fn set_tracing(&mut self, mode: TraceMode) -> &mut Self {
        self.inspector_mut().tracing(mode);
        self
    }

    #[inline]
    pub fn set_trace_printer(&mut self, trace_printer: bool) -> &mut Self {
        self.inspector_mut().print(trace_printer);
        self
    }

    #[inline]
    pub fn set_gas_limit(&mut self, gas_limit: u64) -> &mut Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Deploys a contract and commits the new state to the underlying database.
    ///
    /// Executes a CREATE transaction with the contract `code` and persistent database state
    /// modifications.
    pub fn deploy(
        &mut self,
        from: Address,
        code: Bytes,
        value: U256,
        rd: Option<&RevertDecoder>,
    ) -> Result<DeployResult, EvmError> {
        let env = self.build_test_env(from, TxKind::Create, code, value);
        self.deploy_with_env(env, rd)
    }

    /// Deploys a contract using the given `env` and commits the new state to the underlying
    /// database.
    ///
    /// # Panics
    ///
    /// Panics if `env.tx.transact_to` is not `TxKind::Create(_)`.
    #[instrument(name = "deploy", level = "debug", skip_all)]
    pub fn deploy_with_env(
        &mut self,
        env: EnvWithHandlerCfg,
        rd: Option<&RevertDecoder>,
    ) -> Result<DeployResult, EvmError> {
        assert!(
            matches!(env.tx.transact_to, TxKind::Create),
            "Expected create transaction, got {:?}",
            env.tx.transact_to
        );
        trace!(sender=%env.tx.caller, "deploying contract");

        let mut result = self.transact_with_env(env)?;
        result = result.into_result(rd)?;
        let Some(Output::Create(_, Some(address))) = result.out else {
            panic!("Deployment succeeded, but no address was returned: {result:#?}");
        };

        // also mark this library as persistent, this will ensure that the state of the library is
        // persistent across fork swaps in forking mode
        self.backend_mut().add_persistent_account(address);

        debug!(%address, "deployed contract");

        Ok(DeployResult { raw: result, address })
    }

    /// Calls the `setUp()` function on a contract.
    ///
    /// This will commit any state changes to the underlying database.
    ///
    /// Ayn changes made during the setup call to env's block environment are persistent, for
    /// example `vm.chainId()` will change the `block.chainId` for all subsequent test calls.
    #[instrument(name = "setup", level = "debug", skip_all)]
    pub fn setup(
        &mut self,
        from: Option<Address>,
        to: Address,
        rd: Option<&RevertDecoder>,
    ) -> Result<RawCallResult, EvmError> {
        trace!(?from, ?to, "setting up contract");

        let from = from.unwrap_or(CALLER);
        self.backend_mut().set_caller(from);
        let calldata = Bytes::from_static(&ITest::setUpCall::SELECTOR);
        let mut res = self.transact_raw(from, to, calldata, U256::ZERO)?;
        res = res.into_result(rd)?;

        // record any changes made to the block's environment during setup
        self.env_mut().block = res.env.block.clone();
        // and also the chainid, which can be set manually
        self.env_mut().cfg.chain_id = res.env.cfg.chain_id;

        let success =
            self.is_raw_call_success(to, Cow::Borrowed(&res.state_changeset), &res, false);
        if !success {
            return Err(res.into_execution_error("execution error".to_string()).into());
        }

        Ok(res)
    }

    /// Performs a call to an account on the current state of the VM.
    pub fn call(
        &self,
        from: Address,
        to: Address,
        func: &Function,
        args: &[DynSolValue],
        value: U256,
        rd: Option<&RevertDecoder>,
    ) -> Result<CallResult, EvmError> {
        let calldata = Bytes::from(func.abi_encode_input(args)?);
        let result = self.call_raw(from, to, calldata, value)?;
        result.into_decoded_result(func, rd)
    }

    /// Performs a call to an account on the current state of the VM.
    pub fn call_sol<C: SolCall>(
        &self,
        from: Address,
        to: Address,
        args: &C,
        value: U256,
        rd: Option<&RevertDecoder>,
    ) -> Result<CallResult<C::Return>, EvmError> {
        let calldata = Bytes::from(args.abi_encode());
        let mut raw = self.call_raw(from, to, calldata, value)?;
        raw = raw.into_result(rd)?;
        Ok(CallResult { decoded_result: C::abi_decode_returns(&raw.result, false)?, raw })
    }

    /// Performs a call to an account on the current state of the VM.
    pub fn transact(
        &mut self,
        from: Address,
        to: Address,
        func: &Function,
        args: &[DynSolValue],
        value: U256,
        rd: Option<&RevertDecoder>,
    ) -> Result<CallResult, EvmError> {
        let calldata = Bytes::from(func.abi_encode_input(args)?);
        let result = self.transact_raw(from, to, calldata, value)?;
        result.into_decoded_result(func, rd)
    }

    /// Performs a raw call to an account on the current state of the VM.
    pub fn call_raw(
        &self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
    ) -> eyre::Result<RawCallResult> {
        let env = self.build_test_env(from, TxKind::Call(to), calldata, value);
        self.call_with_env(env)
    }

    /// Performs a raw call to an account on the current state of the VM with an EIP-7702
    /// authorization list.
    pub fn call_raw_with_authorization(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
        authorization_list: Vec<SignedAuthorization>,
    ) -> eyre::Result<RawCallResult> {
        let mut env = self.build_test_env(from, to.into(), calldata, value);
        env.tx.authorization_list = Some(AuthorizationList::Signed(authorization_list));
        self.call_with_env(env)
    }

    /// Performs a raw call to an account on the current state of the VM.
    pub fn transact_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
    ) -> eyre::Result<RawCallResult> {
        let env = self.build_test_env(from, TxKind::Call(to), calldata, value);
        self.transact_with_env(env)
    }

    /// Execute the transaction configured in `env.tx`.
    ///
    /// The state after the call is **not** persisted.
    #[instrument(name = "call", level = "debug", skip_all)]
    pub fn call_with_env(&self, mut env: EnvWithHandlerCfg) -> eyre::Result<RawCallResult> {
        let mut inspector = self.inspector().clone();
        let mut backend = CowBackend::new_borrowed(self.backend());

        let result = self.strategy.runner.call(
            self.strategy.context.as_ref(),
            &mut backend,
            &mut env,
            &self.env,
            &mut inspector,
        )?;

        convert_executed_result(
            env.clone(),
            inspector,
            result,
            backend.has_state_snapshot_failure(),
        )
    }

    /// Execute the transaction configured in `env.tx`.
    #[instrument(name = "transact", level = "debug", skip_all)]
    pub fn transact_with_env(&mut self, mut env: EnvWithHandlerCfg) -> eyre::Result<RawCallResult> {
        let mut inspector = self.inspector.clone();
        let backend = &mut self.backend;

        let result_and_state = self.strategy.runner.transact(
            self.strategy.context.as_mut(),
            backend,
            &mut env,
            &self.env,
            &mut inspector,
        )?;

        let mut result = convert_executed_result(
            env.clone(),
            inspector,
            result_and_state,
            backend.has_state_snapshot_failure(),
        )?;

        self.commit(&mut result);
        Ok(result)
    }

    /// Commit the changeset to the database and adjust `self.inspector_config` values according to
    /// the executed call result.
    ///
    /// This should not be exposed to the user, as it should be called only by `transact*`.
    #[instrument(name = "commit", level = "debug", skip_all)]
    fn commit(&mut self, result: &mut RawCallResult) {
        // Persist changes to db.
        self.backend_mut().commit(result.state_changeset.clone());

        // Persist cheatcode state.
        self.inspector_mut().cheatcodes = result.cheatcodes.take();
        if let Some(cheats) = self.inspector_mut().cheatcodes.as_mut() {
            // Clear broadcastable transactions
            cheats.broadcastable_transactions.clear();
            cheats.ignored_traces.ignored.clear();

            // if tracing was paused but never unpaused, we should begin next frame with tracing
            // still paused
            if let Some(last_pause_call) = cheats.ignored_traces.last_pause_call.as_mut() {
                *last_pause_call = (0, 0);
            }
        }

        // Persist the changed environment.
        self.inspector_mut().set_env(&result.env);
    }

    /// Returns `true` if a test can be considered successful.
    ///
    /// This is the same as [`Self::is_success`], but will consume the `state_changeset` map to use
    /// internally when calling `failed()`.
    pub fn is_raw_call_mut_success(
        &self,
        address: Address,
        call_result: &mut RawCallResult,
        should_fail: bool,
    ) -> bool {
        self.is_raw_call_success(
            address,
            Cow::Owned(std::mem::take(&mut call_result.state_changeset)),
            call_result,
            should_fail,
        )
    }

    /// Returns `true` if a test can be considered successful.
    ///
    /// This is the same as [`Self::is_success`], but intended for outcomes of [`Self::call_raw`].
    pub fn is_raw_call_success(
        &self,
        address: Address,
        state_changeset: Cow<'_, StateChangeset>,
        call_result: &RawCallResult,
        should_fail: bool,
    ) -> bool {
        if call_result.has_state_snapshot_failure {
            // a failure occurred in a reverted snapshot, which is considered a failed test
            return should_fail;
        }
        self.is_success(address, call_result.reverted, state_changeset, should_fail)
    }

    /// Returns `true` if a test can be considered successful.
    ///
    /// If the call succeeded, we also have to check the global and local failure flags.
    ///
    /// These are set by the test contract itself when an assertion fails, using the internal `fail`
    /// function. The global flag is located in [`CHEATCODE_ADDRESS`] at slot [`GLOBAL_FAIL_SLOT`],
    /// and the local flag is located in the test contract at an unspecified slot.
    ///
    /// This behavior is inherited from Dapptools, where initially only a public
    /// `failed` variable was used to track test failures, and later, a global failure flag was
    /// introduced to track failures across multiple contracts in
    /// [ds-test#30](https://github.com/dapphub/ds-test/pull/30).
    ///
    /// The assumption is that the test runner calls `failed` on the test contract to determine if
    /// it failed. However, we want to avoid this as much as possible, as it is relatively
    /// expensive to set up an EVM call just for checking a single boolean flag.
    ///
    /// See:
    /// - Newer DSTest: <https://github.com/dapphub/ds-test/blob/e282159d5170298eb2455a6c05280ab5a73a4ef0/src/test.sol#L47-L63>
    /// - Older DSTest: <https://github.com/dapphub/ds-test/blob/9ca4ecd48862b40d7b0197b600713f64d337af12/src/test.sol#L38-L49>
    /// - forge-std: <https://github.com/foundry-rs/forge-std/blob/19891e6a0b5474b9ea6827ddb90bb9388f7acfc0/src/StdAssertions.sol#L38-L44>
    pub fn is_success(
        &self,
        address: Address,
        reverted: bool,
        state_changeset: Cow<'_, StateChangeset>,
        should_fail: bool,
    ) -> bool {
        let success = self.is_success_raw(address, reverted, state_changeset);
        should_fail ^ success
    }

    #[instrument(name = "is_success", level = "debug", skip_all)]
    fn is_success_raw(
        &self,
        address: Address,
        reverted: bool,
        state_changeset: Cow<'_, StateChangeset>,
    ) -> bool {
        // The call reverted.
        if reverted {
            return false;
        }

        // A failure occurred in a reverted snapshot, which is considered a failed test.
        if self.backend().has_state_snapshot_failure() {
            return false;
        }

        // Check the global failure slot.
        if let Some(acc) = state_changeset.get(&CHEATCODE_ADDRESS) {
            if let Some(failed_slot) = acc.storage.get(&GLOBAL_FAIL_SLOT) {
                if !failed_slot.present_value().is_zero() {
                    return false;
                }
            }
        }
        if let Ok(failed_slot) = self.backend().storage_ref(CHEATCODE_ADDRESS, GLOBAL_FAIL_SLOT) {
            if !failed_slot.is_zero() {
                return false;
            }
        }

        if !self.legacy_assertions {
            return true;
        }

        // Finally, resort to calling `DSTest::failed`.
        {
            // Construct a new bare-bones backend to evaluate success.
            let mut backend = self.backend().clone_empty();

            // We only clone the test contract and cheatcode accounts,
            // that's all we need to evaluate success.
            for address in [address, CHEATCODE_ADDRESS] {
                let Ok(acc) = self.backend().basic_ref(address) else { return false };
                backend.insert_account_info(address, acc.unwrap_or_default());
            }

            // If this test failed any asserts, then this changeset will contain changes
            // `false -> true` for the contract's `failed` variable and the `globalFailure` flag
            // in the state of the cheatcode address,
            // which are both read when we call `"failed()(bool)"` in the next step.
            backend.commit(state_changeset.into_owned());

            // Check if a DSTest assertion failed
            let executor = self.clone_with_backend(backend);
            let call = executor.call_sol(CALLER, address, &ITest::failedCall {}, U256::ZERO, None);
            match call {
                Ok(CallResult { raw: _, decoded_result: ITest::failedReturn { failed } }) => {
                    trace!(failed, "DSTest::failed()");
                    !failed
                }
                Err(err) => {
                    trace!(%err, "failed to call DSTest::failed()");
                    true
                }
            }
        }
    }

    /// Creates the environment to use when executing a transaction in a test context
    ///
    /// If using a backend with cheatcodes, `tx.gas_price` and `block.number` will be overwritten by
    /// the cheatcode state in between calls.
    fn build_test_env(
        &self,
        caller: Address,
        transact_to: TxKind,
        data: Bytes,
        value: U256,
    ) -> EnvWithHandlerCfg {
        let env = Env {
            cfg: self.env().cfg.clone(),
            // We always set the gas price to 0 so we can execute the transaction regardless of
            // network conditions - the actual gas price is kept in `self.block` and is applied by
            // the cheatcode handler if it is enabled
            block: BlockEnv {
                basefee: U256::ZERO,
                gas_limit: U256::from(self.gas_limit),
                ..self.env().block.clone()
            },
            tx: TxEnv {
                caller,
                transact_to,
                data,
                value,
                // As above, we set the gas price to 0.
                gas_price: U256::ZERO,
                gas_priority_fee: None,
                gas_limit: self.gas_limit,
                ..self.env().tx.clone()
            },
        };

        EnvWithHandlerCfg::new_with_spec_id(Box::new(env), self.spec_id())
    }

    pub fn call_sol_default<C: SolCall>(&self, to: Address, args: &C) -> C::Return
    where
        C::Return: Default,
    {
        self.call_sol(CALLER, to, args, U256::ZERO, None)
            .map(|c| c.decoded_result)
            .inspect_err(|e| warn!(target: "forge::test", "failed calling {:?}: {e}", C::SIGNATURE))
            .unwrap_or_default()
    }
}

/// Represents the context after an execution error occurred.
#[derive(Debug, thiserror::Error)]
#[error("execution reverted: {reason} (gas: {})", raw.gas_used)]
pub struct ExecutionErr {
    /// The raw result of the call.
    pub raw: RawCallResult,
    /// The revert reason.
    pub reason: String,
}

impl std::ops::Deref for ExecutionErr {
    type Target = RawCallResult;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

impl std::ops::DerefMut for ExecutionErr {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.raw
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EvmError {
    /// Error which occurred during execution of a transaction.
    #[error(transparent)]
    Execution(#[from] Box<ExecutionErr>),
    /// Error which occurred during ABI encoding/decoding.
    #[error(transparent)]
    Abi(#[from] alloy_dyn_abi::Error),
    /// Error caused which occurred due to calling the `skip` cheatcode.
    #[error("{_0}")]
    Skip(SkipReason),
    /// Any other error.
    #[error(transparent)]
    Eyre(eyre::Error),
}

impl From<ExecutionErr> for EvmError {
    fn from(err: ExecutionErr) -> Self {
        Self::Execution(Box::new(err))
    }
}

impl From<alloy_sol_types::Error> for EvmError {
    fn from(err: alloy_sol_types::Error) -> Self {
        Self::Abi(err.into())
    }
}

impl From<eyre::Error> for EvmError {
    fn from(err: eyre::Report) -> Self {
        let mut chained_cause = String::new();
        for cause in err.chain() {
            chained_cause.push_str(format!("{cause}; ").as_str());
        }
        Self::Eyre(eyre::format_err!("{chained_cause}"))
    }
}

/// The result of a deployment.
#[derive(Debug)]
pub struct DeployResult {
    /// The raw result of the deployment.
    pub raw: RawCallResult,
    /// The address of the deployed contract
    pub address: Address,
}

impl std::ops::Deref for DeployResult {
    type Target = RawCallResult;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

impl std::ops::DerefMut for DeployResult {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.raw
    }
}

impl From<DeployResult> for RawCallResult {
    fn from(d: DeployResult) -> Self {
        d.raw
    }
}

/// The result of a raw call.
#[derive(Debug)]
pub struct RawCallResult {
    /// The status of the call
    pub exit_reason: InstructionResult,
    /// Whether the call reverted or not
    pub reverted: bool,
    /// Whether the call includes a snapshot failure
    ///
    /// This is tracked separately from revert because a snapshot failure can occur without a
    /// revert, since assert failures are stored in a global variable (ds-test legacy)
    pub has_state_snapshot_failure: bool,
    /// The raw result of the call.
    pub result: Bytes,
    /// The gas used for the call
    pub gas_used: u64,
    /// Refunded gas
    pub gas_refunded: u64,
    /// The initial gas stipend for the transaction
    pub stipend: u64,
    /// The logs emitted during the call
    pub logs: Vec<Log>,
    /// The labels assigned to addresses during the call
    pub labels: AddressHashMap<String>,
    /// The traces of the call
    pub traces: Option<SparsedTraceArena>,
    /// The coverage info collected during the call
    pub coverage: Option<HitMaps>,
    /// Scripted transactions generated from this call
    pub transactions: Option<BroadcastableTransactions>,
    /// The changeset of the state.
    pub state_changeset: StateChangeset,
    /// The `revm::Env` after the call
    pub env: EnvWithHandlerCfg,
    /// The cheatcode states after execution
    pub cheatcodes: Option<Cheatcodes>,
    /// The raw output of the execution
    pub out: Option<Output>,
    /// The chisel state
    pub chisel_state: Option<(Vec<U256>, Vec<u8>, InstructionResult)>,
}

impl Default for RawCallResult {
    fn default() -> Self {
        Self {
            exit_reason: InstructionResult::Continue,
            reverted: false,
            has_state_snapshot_failure: false,
            result: Bytes::new(),
            gas_used: 0,
            gas_refunded: 0,
            stipend: 0,
            logs: Vec::new(),
            labels: HashMap::default(),
            traces: None,
            coverage: None,
            transactions: None,
            state_changeset: HashMap::default(),
            env: EnvWithHandlerCfg::new_with_spec_id(Box::default(), SpecId::LATEST),
            cheatcodes: Default::default(),
            out: None,
            chisel_state: None,
        }
    }
}

impl RawCallResult {
    /// Unpacks an EVM result.
    pub fn from_evm_result(r: Result<Self, EvmError>) -> eyre::Result<(Self, Option<String>)> {
        match r {
            Ok(r) => Ok((r, None)),
            Err(EvmError::Execution(e)) => Ok((e.raw, Some(e.reason))),
            Err(e) => Err(e.into()),
        }
    }

    /// Unpacks an execution result.
    pub fn from_execution_result(r: Result<Self, ExecutionErr>) -> (Self, Option<String>) {
        match r {
            Ok(r) => (r, None),
            Err(e) => (e.raw, Some(e.reason)),
        }
    }

    /// Converts the result of the call into an `EvmError`.
    pub fn into_evm_error(self, rd: Option<&RevertDecoder>) -> EvmError {
        if let Some(reason) = SkipReason::decode(&self.result) {
            return EvmError::Skip(reason);
        }
        let reason = rd.unwrap_or_default().decode(&self.result, Some(self.exit_reason));
        EvmError::Execution(Box::new(self.into_execution_error(reason)))
    }

    /// Converts the result of the call into an `ExecutionErr`.
    pub fn into_execution_error(self, reason: String) -> ExecutionErr {
        ExecutionErr { raw: self, reason }
    }

    /// Returns an `EvmError` if the call failed, otherwise returns `self`.
    pub fn into_result(self, rd: Option<&RevertDecoder>) -> Result<Self, EvmError> {
        if self.exit_reason.is_ok() {
            Ok(self)
        } else {
            Err(self.into_evm_error(rd))
        }
    }

    /// Decodes the result of the call with the given function.
    pub fn into_decoded_result(
        mut self,
        func: &Function,
        rd: Option<&RevertDecoder>,
    ) -> Result<CallResult, EvmError> {
        self = self.into_result(rd)?;
        let mut result = func.abi_decode_output(&self.result, false)?;
        let decoded_result = if result.len() == 1 {
            result.pop().unwrap()
        } else {
            // combine results into a tuple
            DynSolValue::Tuple(result)
        };
        Ok(CallResult { raw: self, decoded_result })
    }

    /// Returns the transactions generated from this call.
    pub fn transactions(&self) -> Option<&BroadcastableTransactions> {
        self.cheatcodes.as_ref().map(|c| &c.broadcastable_transactions)
    }
}

/// The result of a call.
pub struct CallResult<T = DynSolValue> {
    /// The raw result of the call.
    pub raw: RawCallResult,
    /// The decoded result of the call.
    pub decoded_result: T,
}

impl std::ops::Deref for CallResult {
    type Target = RawCallResult;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

impl std::ops::DerefMut for CallResult {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.raw
    }
}

/// Converts the data aggregated in the `inspector` and `call` to a `RawCallResult`
fn convert_executed_result(
    env: EnvWithHandlerCfg,
    inspector: InspectorStack,
    ResultAndState { result, state: state_changeset }: ResultAndState,
    has_state_snapshot_failure: bool,
) -> eyre::Result<RawCallResult> {
    let (exit_reason, gas_refunded, gas_used, out, _exec_logs) = match result {
        ExecutionResult::Success { reason, gas_used, gas_refunded, output, logs, .. } => {
            (reason.into(), gas_refunded, gas_used, Some(output), logs)
        }
        ExecutionResult::Revert { gas_used, output } => {
            // Need to fetch the unused gas
            (InstructionResult::Revert, 0_u64, gas_used, Some(Output::Call(output)), vec![])
        }
        ExecutionResult::Halt { reason, gas_used } => {
            (reason.into(), 0_u64, gas_used, None, vec![])
        }
    };
    let stipend = revm::interpreter::gas::validate_initial_tx_gas(
        env.spec_id(),
        &env.tx.data,
        env.tx.transact_to.is_create(),
        &env.tx.access_list,
        0,
    );

    let result = match &out {
        Some(Output::Call(data)) => data.clone(),
        _ => Bytes::new(),
    };

    let InspectorData { logs, labels, traces, coverage, cheatcodes, chisel_state } =
        inspector.collect();

    let transactions = cheatcodes
        .as_ref()
        .map(|c| c.broadcastable_transactions.clone())
        .filter(|txs| !txs.is_empty());

    Ok(RawCallResult {
        exit_reason,
        reverted: !matches!(exit_reason, return_ok!()),
        has_state_snapshot_failure,
        result,
        gas_used,
        gas_refunded,
        stipend,
        logs,
        labels,
        traces,
        coverage,
        transactions,
        state_changeset,
        env,
        cheatcodes,
        out,
        chisel_state,
    })
}
