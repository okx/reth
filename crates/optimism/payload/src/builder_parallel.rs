//! Parallel execution integration for the OP payload builder.
//!
//! When enabled via `--xlayer.parallel-exec`, this module pre-simulates mempool
//! transactions using the parallel execution framework's [`Simulator`] to:
//! 1. Pre-filter transactions that would fail during execution
//! 2. Pre-warm state caches via simulation reads
//! 3. Log conflict/framing statistics for observability
//!
//! Phase 1: pre-simulation + filtering, sequential execution through BlockBuilder
//! Phase 2 (future): full parallel dispatch with direct receipt construction

use crate::{
    builder::{ExecutionInfo, OpPayloadBuilderCtx},
    intercept_bridge_transaction_if_need, OpAttributes, OpPayloadPrimitives,
};
use alloy_consensus::{transaction::TxHashRef, Transaction, Typed2718};
use alloy_evm::{block::CommitChanges, Evm as AlloyEvm};
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    execute::{BlockBuilder, BlockExecutionError, BlockExecutor, BlockValidationError},
    op_revm::L1BlockInfo,
    ConfigureEvm, Database,
};
use reth_node_metrics::transaction_trace_xlayer::{get_global_tracer, TransactionProcessId};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::transaction::OpTransaction;
use reth_optimism_txpool::{
    estimated_da_size::DataAvailabilitySized,
    interop::{is_valid_interop, MaybeInteropTransaction},
    OpPooledTx,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::BuildNextEnv;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{HeaderTy, TxTy};
use reth_storage_api::{errors::ProviderError, StateProvider};
use reth_transaction_pool::PoolTransaction;
use revm::context::{result::ExecutionResult, Block, BlockEnv, TxEnv};
use std::collections::HashMap;
use tracing::{debug, trace};
use xlayer_parallel_exec::simulator::{SimTxEnv, Simulator};

/// A [`DatabaseRef`](revm::DatabaseRef) adapter with account state overlay.
///
/// Reads account info from `account_overrides` first (post-sequencer state),
/// then falls back to the base `StateProvider` (pre-block state).
///
/// This solves a critical consistency issue: the `StateProvider` from
/// `state_by_block_hash()` reflects the parent block's state. After sequencer
/// transactions execute (L1 deposits, L1BlockInfo updates), accounts may have
/// updated balances/nonces that are not visible through the raw `StateProvider`.
/// By pre-reading these accounts from the builder's `State<DB>` and storing
/// them as overrides, the simulation sees the correct post-sequencer state.
///
/// For bytecodes, storage, and block hashes, reads go directly to the
/// `StateProvider` since sequencer transactions rarely modify arbitrary storage
/// slots that mempool transactions depend on.
struct SimDatabaseRef<'a> {
    /// Account state overrides from post-sequencer execution.
    /// Pre-read from `builder.evm_mut().db_mut()` which includes all state
    /// changes from pre-execution and sequencer transactions.
    account_overrides: HashMap<Address, Option<revm::state::AccountInfo>>,
    /// Base state provider (parent block state, does NOT include sequencer changes).
    provider: &'a dyn StateProvider,
}

impl core::fmt::Debug for SimDatabaseRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimDatabaseRef")
            .field("account_overrides", &self.account_overrides.len())
            .finish_non_exhaustive()
    }
}

impl revm::DatabaseRef for SimDatabaseRef<'_> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        // Check post-sequencer override first
        if let Some(info) = self.account_overrides.get(&address) {
            return Ok(info.clone());
        }
        // Fall back to base state provider (parent block state)
        Ok(self.provider.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(self.provider.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.provider.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        Ok(reth_storage_api::BlockHashReader::block_hash(self.provider, number)?
            .unwrap_or_default())
    }
}

/// Convert a consensus transaction into a [`SimTxEnv`] for pre-simulation.
///
/// Extracts basic transaction fields needed for EVM simulation. Fields like
/// access lists and EIP-1559 priority fees are omitted since the simulation
/// only needs to determine success/failure and read/write sets.
fn consensus_tx_to_sim_env<T: Transaction>(signer: Address, tx: &T) -> SimTxEnv {
    let tx_env = TxEnv {
        caller: signer,
        gas_limit: tx.gas_limit(),
        gas_price: tx.gas_price().unwrap_or_default(),
        kind: tx.to().into(),
        value: tx.value(),
        data: tx.input().clone(),
        nonce: tx.nonce(),
        ..Default::default()
    };
    SimTxEnv { sender: signer, tx_env }
}

impl<Evm, ChainSpec, Attrs> OpPayloadBuilderCtx<Evm, ChainSpec, Attrs>
where
    Evm: ConfigureEvm<
        Primitives: OpPayloadPrimitives,
        NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<Evm::Primitives>, ChainSpec>,
    >,
    ChainSpec: EthChainSpec + OpHardforks,
    Attrs: OpAttributes<Transaction = TxTy<Evm::Primitives>>,
{
    /// Execute best transactions with parallel pre-simulation and filtering.
    ///
    /// Phase 1 strategy:
    /// 1. Collect all candidate transactions from the pool
    /// 2. Pre-simulate each using the parallel framework's [`Simulator`]
    /// 3. Filter out transactions that failed simulation (would revert/fail)
    /// 4. Execute surviving transactions through the existing sequential path with bridge
    ///    interception
    ///
    /// This provides two benefits over the pure sequential path:
    /// - Failed transactions are skipped without paying full EVM execution cost
    /// - State reads during simulation pre-warm provider caches
    pub fn execute_best_transactions_parallel<Builder>(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut Builder,
        mut best_txs: impl PayloadTransactions<
            Transaction: PoolTransaction<Consensus = TxTy<Evm::Primitives>> + OpPooledTx,
        >,
        state_provider: &dyn StateProvider,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        Builder: BlockBuilder<Primitives = Evm::Primitives>,
        <<Builder::Executor as BlockExecutor>::Evm as AlloyEvm>::DB: Database,
    {
        let block_gas_limit = {
            let mut limit = builder.evm_mut().block().gas_limit();
            if let Some(gas_limit_config) = self.builder_config.gas_limit_config.gas_limit() {
                limit = gas_limit_config.min(limit);
            }
            limit
        };
        let block_da_limit = self.builder_config.da_config.max_da_block_size();
        let tx_da_limit = self.builder_config.da_config.max_da_tx_size();
        let base_fee = builder.evm_mut().block().basefee();
        let block_number: u64 = builder.evm_mut().block().number().saturating_to();

        // --- Phase 1: Collect and pre-simulate ---

        // Collect all candidate transactions from the pool
        let mut candidates: Vec<_> = Vec::new();
        while let Some(tx) = best_txs.next(()) {
            let interop = tx.interop_deadline();
            let tx_da_size = tx.estimated_da_size();
            let consensus_tx = tx.into_consensus();

            // Skip blob and deposit transactions
            if consensus_tx.is_eip4844() || consensus_tx.is_deposit() {
                best_txs.mark_invalid(consensus_tx.signer(), consensus_tx.nonce());
                continue;
            }

            // Skip invalid cross-chain txs
            if let Some(interop) = interop &&
                !is_valid_interop(interop, self.config.attributes.timestamp())
            {
                best_txs.mark_invalid(consensus_tx.signer(), consensus_tx.nonce());
                continue;
            }

            candidates.push((consensus_tx, tx_da_size));
        }

        if candidates.is_empty() {
            return Ok(None);
        }

        // Build SimTxEnvs for pre-simulation
        let sim_txs: Vec<SimTxEnv> =
            candidates.iter().map(|(tx, _)| consensus_tx_to_sim_env(tx.signer(), &**tx)).collect();

        // Pre-read sender accounts from post-sequencer state (builder's State<DB>)
        // so simulation sees correct balances/nonces after L1 deposits execute.
        let account_overrides = {
            let mut overrides = HashMap::new();
            let db = builder.evm_mut().db_mut();
            for (tx, _) in &candidates {
                let sender = tx.signer();
                if let std::collections::hash_map::Entry::Vacant(e) = overrides.entry(sender) {
                    if let Ok(info) = revm::Database::basic(db, sender) {
                        e.insert(info);
                    }
                }
            }
            overrides
        };

        // Pre-simulate using the parallel framework's Simulator
        let simulator = Simulator::new();
        let sim_db = SimDatabaseRef { account_overrides, provider: state_provider };
        let block_env = BlockEnv {
            number: builder.evm_mut().block().number().saturating_to(),
            beneficiary: builder.evm_mut().block().beneficiary(),
            timestamp: builder.evm_mut().block().timestamp().saturating_to(),
            gas_limit: builder.evm_mut().block().gas_limit(),
            basefee: builder.evm_mut().block().basefee(),
            ..Default::default()
        };
        let sim_results = simulator.simulate(&sim_txs, &sim_db, &block_env);

        let total_candidates = candidates.len();
        let sim_success_count = sim_results.iter().filter(|r| r.success).count();
        debug!(
            target: "payload_builder::parallel",
            total_candidates,
            sim_success_count,
            sim_failed = total_candidates - sim_success_count,
            "pre-simulation complete"
        );

        // --- Phase 1: Execute surviving transactions sequentially ---

        for (idx, ((consensus_tx, tx_da_size), sim_result)) in
            candidates.into_iter().zip(sim_results.iter()).enumerate()
        {
            // Check cancellation
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            // Skip transactions that failed simulation
            if !sim_result.success {
                trace!(
                    target: "payload_builder::parallel",
                    tx_idx = idx,
                    tx_hash = ?consensus_tx.tx_hash(),
                    "skipping transaction that failed pre-simulation"
                );
                best_txs.mark_invalid(consensus_tx.signer(), consensus_tx.nonce());
                continue;
            }

            let da_footprint_gas_scalar = self
                .chain_spec
                .is_jovian_active_at_timestamp(self.attributes().timestamp())
                .then_some(
                    L1BlockInfo::fetch_da_footprint_gas_scalar(builder.evm_mut().db_mut()).expect(
                        "DA footprint should always be available from the database post jovian",
                    ),
                );

            // Check gas/DA limits
            if info.is_tx_over_limits(
                tx_da_size,
                block_gas_limit,
                tx_da_limit,
                block_da_limit,
                consensus_tx.gas_limit(),
                da_footprint_gas_scalar,
            ) {
                best_txs.mark_invalid(consensus_tx.signer(), consensus_tx.nonce());
                continue;
            }

            // Execute through BlockBuilder with bridge interception
            let signer = consensus_tx.signer();
            let nonce = consensus_tx.nonce();
            let tx_hash = *consensus_tx.tx_hash();
            let miner_fee = consensus_tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");

            let gas_used = match builder.execute_transaction_with_commit_condition(
                consensus_tx,
                |result| {
                    if let ExecutionResult::Success { logs, .. } = result {
                        if intercept_bridge_transaction_if_need(
                            logs,
                            signer,
                            &self.bridge_intercept,
                        )
                        .is_err()
                        {
                            return CommitChanges::No;
                        }
                    }
                    CommitChanges::Yes
                },
            ) {
                Ok(Some(gas_used)) => {
                    if let Some(tracer) = get_global_tracer() {
                        tracer.log_transaction(
                            tx_hash,
                            TransactionProcessId::SeqTxExecutionEnd,
                            Some(block_number),
                        );
                    }
                    gas_used
                }
                Ok(None) => {
                    if let Some(tracer) = get_global_tracer() {
                        tracer.log_transaction(
                            tx_hash,
                            TransactionProcessId::SeqTxExecutionEnd,
                            Some(block_number),
                        );
                    }
                    trace!(target: "payload_builder::parallel", ?tx_hash, "bridge transaction intercepted");
                    best_txs.mark_invalid(signer, nonce);
                    continue;
                }
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if let Some(tracer) = get_global_tracer() {
                        tracer.log_transaction(
                            tx_hash,
                            TransactionProcessId::SeqTxExecutionEnd,
                            Some(block_number),
                        );
                    }
                    if error.is_nonce_too_low() {
                        trace!(target: "payload_builder::parallel", %error, ?tx_hash, "skipping nonce too low transaction");
                    } else {
                        trace!(target: "payload_builder::parallel", %error, ?tx_hash, "skipping invalid transaction and its descendants");
                        best_txs.mark_invalid(signer, nonce);
                    }
                    continue;
                }
                Err(err) => {
                    if let Some(tracer) = get_global_tracer() {
                        tracer.log_transaction(
                            tx_hash,
                            TransactionProcessId::SeqTxExecutionEnd,
                            Some(block_number),
                        );
                    }
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            info.cumulative_gas_used += gas_used;
            info.cumulative_da_bytes_used += tx_da_size;
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, StorageKey, StorageValue, TxKind};
    use reth_primitives_traits::{Account, Bytecode};
    use reth_storage_api::{
        errors::ProviderResult, AccountReader, BlockHashReader, BytecodeReader,
        HashedPostStateProvider, StateProofProvider, StateRootProvider, StorageRootProvider,
    };
    use reth_trie_common::{
        updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
        MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    };
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // TestStateProvider: minimal mock for testing SimDatabaseRef
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct TestStateProvider {
        accounts: HashMap<Address, Account>,
        storage: HashMap<(Address, StorageKey), StorageValue>,
        bytecodes: HashMap<B256, Bytecode>,
        block_hashes: HashMap<u64, B256>,
    }

    impl TestStateProvider {
        fn new() -> Self {
            Self {
                accounts: HashMap::new(),
                storage: HashMap::new(),
                bytecodes: HashMap::new(),
                block_hashes: HashMap::new(),
            }
        }

        fn with_account(mut self, address: Address, balance: U256, nonce: u64) -> Self {
            self.accounts.insert(address, Account { balance, nonce, bytecode_hash: None });
            self
        }

        fn with_storage(mut self, address: Address, slot: StorageKey, value: StorageValue) -> Self {
            self.storage.insert((address, slot), value);
            self
        }

        fn with_block_hash(mut self, number: u64, hash: B256) -> Self {
            self.block_hashes.insert(number, hash);
            self
        }
    }

    impl StateProvider for TestStateProvider {
        fn storage(
            &self,
            address: Address,
            storage_key: StorageKey,
        ) -> ProviderResult<Option<StorageValue>> {
            Ok(self.storage.get(&(address, storage_key)).copied())
        }
    }

    impl BytecodeReader for TestStateProvider {
        fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
            Ok(self.bytecodes.get(code_hash).cloned())
        }
    }

    impl BlockHashReader for TestStateProvider {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
            Ok(self.block_hashes.get(&number).copied())
        }

        fn canonical_hashes_range(&self, _start: u64, _end: u64) -> ProviderResult<Vec<B256>> {
            Ok(vec![])
        }
    }

    impl AccountReader for TestStateProvider {
        fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
            Ok(self.accounts.get(address).cloned())
        }
    }

    impl StateRootProvider for TestStateProvider {
        fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
            Ok(B256::ZERO)
        }

        fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
            Ok(B256::ZERO)
        }

        fn state_root_with_updates(
            &self,
            _hashed_state: HashedPostState,
        ) -> ProviderResult<(B256, TrieUpdates)> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }

        fn state_root_from_nodes_with_updates(
            &self,
            _input: TrieInput,
        ) -> ProviderResult<(B256, TrieUpdates)> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }
    }

    impl StorageRootProvider for TestStateProvider {
        fn storage_root(
            &self,
            _address: Address,
            _hashed_storage: HashedStorage,
        ) -> ProviderResult<B256> {
            Ok(B256::ZERO)
        }

        fn storage_proof(
            &self,
            _address: Address,
            slot: B256,
            _hashed_storage: HashedStorage,
        ) -> ProviderResult<StorageProof> {
            Ok(StorageProof::new(slot))
        }

        fn storage_multiproof(
            &self,
            _address: Address,
            _slots: &[B256],
            _hashed_storage: HashedStorage,
        ) -> ProviderResult<StorageMultiProof> {
            Ok(StorageMultiProof::empty())
        }
    }

    impl StateProofProvider for TestStateProvider {
        fn proof(
            &self,
            _input: TrieInput,
            _address: Address,
            _slots: &[B256],
        ) -> ProviderResult<AccountProof> {
            Ok(AccountProof::new(Address::ZERO))
        }

        fn multiproof(
            &self,
            _input: TrieInput,
            _targets: MultiProofTargets,
        ) -> ProviderResult<MultiProof> {
            Ok(MultiProof::default())
        }

        fn witness(
            &self,
            _input: TrieInput,
            _target: HashedPostState,
        ) -> ProviderResult<Vec<Bytes>> {
            Ok(Vec::default())
        }
    }

    impl HashedPostStateProvider for TestStateProvider {
        fn hashed_post_state(&self, _bundle_state: &revm_database::BundleState) -> HashedPostState {
            HashedPostState::default()
        }
    }

    // -----------------------------------------------------------------------
    // Tests for consensus_tx_to_sim_env
    // -----------------------------------------------------------------------

    #[test]
    fn test_consensus_tx_to_sim_env_basic_fields() {
        let sender = Address::with_last_byte(0xA0);
        let to = Address::with_last_byte(0xB0);
        let value = U256::from(1000);
        let nonce = 5u64;
        let gas_limit = 50000u64;

        let sim = consensus_tx_to_sim_env(
            sender,
            &op_alloy_consensus::OpTypedTransaction::Legacy(alloy_consensus::TxLegacy {
                chain_id: Some(1),
                nonce,
                gas_price: 10,
                gas_limit,
                to: TxKind::Call(to),
                value,
                input: alloy_primitives::Bytes::from_static(b"hello"),
            }),
        );

        assert_eq!(sim.sender, sender);
        assert_eq!(sim.tx_env.caller, sender);
        assert_eq!(sim.tx_env.gas_limit, gas_limit);
        assert_eq!(sim.tx_env.gas_price, 10);
        assert_eq!(sim.tx_env.kind, TxKind::Call(to));
        assert_eq!(sim.tx_env.value, value);
        assert_eq!(sim.tx_env.nonce, nonce);
        assert_eq!(sim.tx_env.data, alloy_primitives::Bytes::from_static(b"hello"));
    }

    #[test]
    fn test_consensus_tx_to_sim_env_zero_values() {
        let sim = consensus_tx_to_sim_env(
            Address::ZERO,
            &op_alloy_consensus::OpTypedTransaction::Legacy(alloy_consensus::TxLegacy {
                chain_id: None,
                nonce: 0,
                gas_price: 0,
                gas_limit: 21000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: alloy_primitives::Bytes::new(),
            }),
        );

        assert_eq!(sim.sender, Address::ZERO);
        assert_eq!(sim.tx_env.value, U256::ZERO);
        assert_eq!(sim.tx_env.nonce, 0);
        assert_eq!(sim.tx_env.gas_price, 0);
    }

    #[test]
    fn test_consensus_tx_to_sim_env_create_tx() {
        let sender = Address::with_last_byte(0xA0);

        let sim = consensus_tx_to_sim_env(
            sender,
            &op_alloy_consensus::OpTypedTransaction::Legacy(alloy_consensus::TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 0,
                gas_limit: 100_000,
                to: TxKind::Create,
                value: U256::ZERO,
                input: alloy_primitives::Bytes::from_static(&[0x60, 0x00, 0x60, 0x00]),
            }),
        );

        assert_eq!(sim.tx_env.kind, TxKind::Create);
        assert_eq!(sim.tx_env.data.len(), 4);
    }

    #[test]
    fn test_consensus_tx_to_sim_env_sender_matches_caller() {
        let sender = Address::with_last_byte(0xFF);
        let sim = consensus_tx_to_sim_env(
            sender,
            &op_alloy_consensus::OpTypedTransaction::Legacy(alloy_consensus::TxLegacy {
                chain_id: Some(1),
                nonce: 42,
                gas_price: 100,
                gas_limit: 21000,
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::from(1),
                input: alloy_primitives::Bytes::new(),
            }),
        );

        assert_eq!(sim.sender, sim.tx_env.caller, "sender and caller must match");
    }

    // -----------------------------------------------------------------------
    // Tests for SimDatabaseRef
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_database_ref_basic_account() {
        use revm::DatabaseRef;

        let addr = Address::with_last_byte(0xA0);
        let provider = TestStateProvider::new().with_account(addr, U256::from(1000), 5);
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.basic_ref(addr).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.balance, U256::from(1000));
        assert_eq!(info.nonce, 5);
    }

    #[test]
    fn test_sim_database_ref_missing_account() {
        use revm::DatabaseRef;

        let provider = TestStateProvider::new();
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.basic_ref(Address::with_last_byte(0x99)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_sim_database_ref_storage() {
        use revm::DatabaseRef;

        let addr = Address::with_last_byte(0xA0);
        let slot = B256::with_last_byte(7);
        let value = U256::from(42);
        let provider = TestStateProvider::new().with_storage(addr, slot, StorageValue::from(value));
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.storage_ref(addr, U256::from(7)).unwrap();
        assert_eq!(result, value);
    }

    #[test]
    fn test_sim_database_ref_missing_storage() {
        use revm::DatabaseRef;

        let provider = TestStateProvider::new();
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.storage_ref(Address::with_last_byte(0xA0), U256::from(7)).unwrap();
        assert_eq!(result, U256::ZERO);
    }

    #[test]
    fn test_sim_database_ref_block_hash() {
        use revm::DatabaseRef;

        let hash = B256::with_last_byte(0xAB);
        let provider = TestStateProvider::new().with_block_hash(42, hash);
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.block_hash_ref(42).unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn test_sim_database_ref_missing_block_hash() {
        use revm::DatabaseRef;

        let provider = TestStateProvider::new();
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.block_hash_ref(999).unwrap();
        assert_eq!(result, B256::ZERO);
    }

    #[test]
    fn test_sim_database_ref_code_by_hash_missing() {
        use revm::DatabaseRef;

        let provider = TestStateProvider::new();
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };

        let result = db.code_by_hash_ref(B256::with_last_byte(0x42)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_sim_database_ref_debug() {
        let provider = TestStateProvider::new();
        let db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };
        let debug_str = format!("{:?}", db);
        assert!(debug_str.contains("SimDatabaseRef"));
    }

    // -----------------------------------------------------------------------
    // Integration test: SimDatabaseRef + Simulator pipeline
    // -----------------------------------------------------------------------

    #[test]
    fn test_simulator_with_sim_database_ref() {
        let sender = Address::with_last_byte(0xA0);
        let recipient = Address::with_last_byte(0xB0);

        let provider = TestStateProvider::new().with_account(sender, U256::from(1_000_000), 0);

        let sim_db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };
        let simulator = Simulator::new();
        let block_env = BlockEnv::default();

        let tx = SimTxEnv {
            sender,
            tx_env: TxEnv {
                caller: sender,
                gas_limit: 100_000,
                gas_price: 0,
                kind: TxKind::Call(recipient),
                value: U256::from(100),
                nonce: 0,
                ..Default::default()
            },
        };

        let results = simulator.simulate(&[tx], &sim_db, &block_env);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].original_index, 0);
        assert!(results[0].success, "transfer from funded account should succeed");
    }

    #[test]
    fn test_simulator_filters_failing_txs() {
        let funded_sender = Address::with_last_byte(0xA0);
        let unfunded_sender = Address::with_last_byte(0xA1);
        let recipient = Address::with_last_byte(0xB0);

        let provider =
            TestStateProvider::new().with_account(funded_sender, U256::from(1_000_000), 0);

        let sim_db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };
        let simulator = Simulator::new();
        let block_env = BlockEnv::default();

        let txs = vec![
            // tx0: funded sender with gas_price=0 → should succeed
            SimTxEnv {
                sender: funded_sender,
                tx_env: TxEnv {
                    caller: funded_sender,
                    gas_limit: 100_000,
                    gas_price: 0,
                    kind: TxKind::Call(recipient),
                    value: U256::from(100),
                    nonce: 0,
                    ..Default::default()
                },
            },
            // tx1: unfunded sender with gas_price > 0 → should fail (can't pay gas)
            SimTxEnv {
                sender: unfunded_sender,
                tx_env: TxEnv {
                    caller: unfunded_sender,
                    gas_limit: 100_000,
                    gas_price: 10,
                    kind: TxKind::Call(recipient),
                    value: U256::from(100),
                    nonce: 0,
                    ..Default::default()
                },
            },
        ];

        let results = simulator.simulate(&txs, &sim_db, &block_env);
        assert_eq!(results.len(), 2);

        assert!(results[0].success, "funded tx should succeed");
        assert!(!results[1].success, "unfunded tx with gas_price > 0 should fail");

        let success_count = results.iter().filter(|r| r.success).count();
        assert_eq!(success_count, 1);
    }

    #[test]
    fn test_simulator_with_multiple_accounts() {
        let sender_a = Address::with_last_byte(0xA0);
        let sender_b = Address::with_last_byte(0xA1);
        let recipient = Address::with_last_byte(0xB0);

        let provider = TestStateProvider::new()
            .with_account(sender_a, U256::from(1_000_000), 0)
            .with_account(sender_b, U256::from(2_000_000), 3);

        let sim_db = SimDatabaseRef { account_overrides: HashMap::new(), provider: &provider };
        let simulator = Simulator::new();
        let block_env = BlockEnv::default();

        let txs = vec![
            SimTxEnv {
                sender: sender_a,
                tx_env: TxEnv {
                    caller: sender_a,
                    gas_limit: 100_000,
                    gas_price: 0,
                    kind: TxKind::Call(recipient),
                    value: U256::from(500),
                    nonce: 0,
                    ..Default::default()
                },
            },
            SimTxEnv {
                sender: sender_b,
                tx_env: TxEnv {
                    caller: sender_b,
                    gas_limit: 100_000,
                    gas_price: 0,
                    kind: TxKind::Call(recipient),
                    value: U256::from(1000),
                    nonce: 3,
                    ..Default::default()
                },
            },
        ];

        let results = simulator.simulate(&txs, &sim_db, &block_env);
        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert!(!results[0].crw_sets.account_writes.is_empty());
        assert!(!results[1].crw_sets.account_writes.is_empty());
    }

    // -----------------------------------------------------------------------
    // Tests for account_overrides (post-sequencer state overlay)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_database_ref_override_takes_priority() {
        use revm::DatabaseRef;

        let addr = Address::with_last_byte(0xA0);
        // Provider has old state: balance=100, nonce=0
        let provider = TestStateProvider::new().with_account(addr, U256::from(100), 0);

        // Override with post-sequencer state: balance=9999, nonce=5
        let mut overrides = HashMap::new();
        overrides.insert(
            addr,
            Some(revm::state::AccountInfo {
                balance: U256::from(9999),
                nonce: 5,
                code_hash: B256::ZERO,
                code: None,
                account_id: None,
            }),
        );

        let db = SimDatabaseRef { account_overrides: overrides, provider: &provider };
        let result = db.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(9999), "override balance should take priority");
        assert_eq!(result.nonce, 5, "override nonce should take priority");
    }

    #[test]
    fn test_sim_database_ref_override_none_means_no_account() {
        use revm::DatabaseRef;

        let addr = Address::with_last_byte(0xA0);
        // Provider has the account
        let provider = TestStateProvider::new().with_account(addr, U256::from(100), 0);

        // Override says account doesn't exist (None)
        let mut overrides = HashMap::new();
        overrides.insert(addr, None);

        let db = SimDatabaseRef { account_overrides: overrides, provider: &provider };
        let result = db.basic_ref(addr).unwrap();
        assert!(result.is_none(), "None override should mean account doesn't exist");
    }

    #[test]
    fn test_sim_database_ref_falls_through_without_override() {
        use revm::DatabaseRef;

        let addr_overridden = Address::with_last_byte(0xA0);
        let addr_not_overridden = Address::with_last_byte(0xB0);

        let provider = TestStateProvider::new()
            .with_account(addr_overridden, U256::from(100), 0)
            .with_account(addr_not_overridden, U256::from(200), 1);

        let mut overrides = HashMap::new();
        overrides.insert(
            addr_overridden,
            Some(revm::state::AccountInfo {
                balance: U256::from(9999),
                nonce: 5,
                code_hash: B256::ZERO,
                code: None,
                account_id: None,
            }),
        );

        let db = SimDatabaseRef { account_overrides: overrides, provider: &provider };

        // addr_not_overridden should fall through to provider
        let result = db.basic_ref(addr_not_overridden).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(200), "non-overridden address should use provider");
        assert_eq!(result.nonce, 1);
    }

    #[test]
    fn test_sim_database_ref_storage_not_affected_by_overrides() {
        use revm::DatabaseRef;

        let addr = Address::with_last_byte(0xA0);
        let slot = B256::with_last_byte(1);
        let provider =
            TestStateProvider::new().with_storage(addr, slot, StorageValue::from(U256::from(42)));

        // Even with account override, storage still comes from provider
        let mut overrides = HashMap::new();
        overrides.insert(
            addr,
            Some(revm::state::AccountInfo {
                balance: U256::from(9999),
                nonce: 5,
                code_hash: B256::ZERO,
                code: None,
                account_id: None,
            }),
        );

        let db = SimDatabaseRef { account_overrides: overrides, provider: &provider };
        let result = db.storage_ref(addr, U256::from(1)).unwrap();
        assert_eq!(result, U256::from(42), "storage should come from provider regardless");
    }

    #[test]
    fn test_simulator_with_overridden_balance() {
        let sender = Address::with_last_byte(0xA0);
        let recipient = Address::with_last_byte(0xB0);

        // Provider has no balance for sender
        let provider = TestStateProvider::new();

        // But override gives sender enough balance
        let mut overrides = HashMap::new();
        overrides.insert(
            sender,
            Some(revm::state::AccountInfo {
                balance: U256::from(1_000_000),
                nonce: 0,
                code_hash: B256::ZERO,
                code: None,
                account_id: None,
            }),
        );

        let sim_db = SimDatabaseRef { account_overrides: overrides, provider: &provider };
        let simulator = Simulator::new();
        let block_env = BlockEnv::default();

        let tx = SimTxEnv {
            sender,
            tx_env: TxEnv {
                caller: sender,
                gas_limit: 100_000,
                gas_price: 0,
                kind: TxKind::Call(recipient),
                value: U256::from(100),
                nonce: 0,
                ..Default::default()
            },
        };

        let results = simulator.simulate(&[tx], &sim_db, &block_env);
        assert_eq!(results.len(), 1);
        assert!(
            results[0].success,
            "tx should succeed with overridden balance even though provider has no account"
        );
    }
}
