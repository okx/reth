//! Parallel payload builder for the QMDB node.
//!
//! Uses the `ParallelExecutionPipeline` for true parallel transaction execution,
//! then assembles the block manually — no re-execution through reth's sequential
//! builder. reth's pipeline is NOT modified; this is a self-contained replacement
//! for the transaction execution + block assembly phase.
//!
//! Flow:
//! 1. Standard builder for pre-execution changes (beacon root, system calls)
//! 2. Extract State<DB> back via `into_executor → finish → finish`
//! 3. Collect transactions from pool (with gas/blob/rlp validation)
//! 4. Execute transactions in parallel via `ParallelExecutionPipeline`
//! 5. Commit parallel results to State<DB> → BundleState
//! 6. Build receipts from ExecutionResult
//! 7. Assemble block header and return EthBuiltPayload

use alloy_consensus::{
    proofs, transaction::Recovered, Block, BlockBody, BlockHeader, Header, Transaction, TxReceipt,
    EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::merge::BEACON_NONCE;
use alloy_primitives::U256;
use alloy_rlp::Encodable;
use reth_basic_payload_builder::{
    is_better_payload, BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::{EthPrimitives, Receipt, TransactionSigned};
use reth_evm::{
    execute::BlockBuilder, spec_by_timestamp_and_block_number, ConfigureEvm,
    NextBlockEnvAttributes, ToTxEnv,
};
use reth_execution_types::BlockExecutionResult;
use reth_node_metrics::block_timing::{BlockTimingContext, BlockTimingPrometheusMetrics};
use reth_payload_builder::{BlobSidecars, EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadBuilderAttributes};
use reth_primitives_traits::{logs_bloom, RecoveredBlock};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};
use reth_trie_common::{updates::TrieUpdates, HashedPostState};
use revm_database::{states::bundle_state::BundleRetention, DatabaseCommit};
use std::sync::Arc;
use tracing::{debug, info, warn};

use xlayer_parallel_exec::pipeline::{ParallelExecutionPipeline, PipelineTxInput};

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// A `DatabaseRef` adapter that reads from a reth `StateProvider`.
///
/// Used as the fallback DB when QMDB is not available.
struct SimDatabaseRef<'a> {
    provider: &'a dyn reth_storage_api::StateProvider,
}

unsafe impl Sync for SimDatabaseRef<'_> {}

impl core::fmt::Debug for SimDatabaseRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimDatabaseRef").finish_non_exhaustive()
    }
}

impl revm::DatabaseRef for SimDatabaseRef<'_> {
    type Error = reth_storage_api::errors::ProviderError;

    fn basic_ref(
        &self,
        address: alloy_primitives::Address,
    ) -> Result<Option<revm::state::AccountInfo>, reth_storage_api::errors::ProviderError> {
        Ok(self.provider.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(
        &self,
        code_hash: alloy_primitives::B256,
    ) -> Result<revm::bytecode::Bytecode, reth_storage_api::errors::ProviderError> {
        Ok(self.provider.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(
        &self,
        address: alloy_primitives::Address,
        index: U256,
    ) -> Result<U256, reth_storage_api::errors::ProviderError> {
        use alloy_primitives::B256;
        Ok(self.provider.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(
        &self,
        number: u64,
    ) -> Result<alloy_primitives::B256, reth_storage_api::errors::ProviderError> {
        Ok(reth_storage_api::BlockHashReader::block_hash(self.provider, number)?
            .unwrap_or_default())
    }
}

/// Direct QMDB `DatabaseRef` — reduces indirection vs going through StateProvider.
///
/// Read path matches fafo's `BlockContext` pattern:
///   1. reth in-memory state (like fafo's curr_state/prev_state)
///   2. QmdbStore direct read (like fafo's ads.read_entry)
///
/// vs old path:
///   SimDatabaseRef → StateProvider(vtable) → QmdbStateProvider
///   → fallback(vtable) → QmdbStore
///
/// Eliminates: QmdbStateProvider layer, 1 vtable dispatch.
/// Bytecodes and block hashes still go through StateProvider.
struct QmdbDirectDbRef<'a> {
    store: &'a xlayer_qmdb_provider::QmdbStore,
    provider: &'a dyn reth_storage_api::StateProvider,
}

unsafe impl Sync for QmdbDirectDbRef<'_> {}

impl core::fmt::Debug for QmdbDirectDbRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QmdbDirectDbRef").finish_non_exhaustive()
    }
}

impl revm::DatabaseRef for QmdbDirectDbRef<'_> {
    type Error = reth_storage_api::errors::ProviderError;

    fn basic_ref(
        &self,
        address: alloy_primitives::Address,
    ) -> Result<Option<revm::state::AccountInfo>, reth_storage_api::errors::ProviderError> {
        // Check reth in-memory state first (recent canonical blocks, like fafo's curr_state)
        if let Some(account) = self.provider.basic_account(&address)? {
            return Ok(Some(account.into()));
        }
        // Direct QMDB read (like fafo's ads.read_entry)
        Ok(self.store.read_account(&address).map(Into::into))
    }

    fn code_by_hash_ref(
        &self,
        code_hash: alloy_primitives::B256,
    ) -> Result<revm::bytecode::Bytecode, reth_storage_api::errors::ProviderError> {
        // Bytecodes are not in QMDB — use StateProvider
        Ok(self.provider.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(
        &self,
        address: alloy_primitives::Address,
        index: U256,
    ) -> Result<U256, reth_storage_api::errors::ProviderError> {
        // Check reth in-memory state first
        use alloy_primitives::B256;
        let slot = B256::new(index.to_be_bytes());
        if let Some(value) = self.provider.storage(address, slot)? {
            return Ok(value);
        }
        // Direct QMDB read
        Ok(self.store.read_storage(&address, &slot).unwrap_or_default())
    }

    fn block_hash_ref(
        &self,
        number: u64,
    ) -> Result<alloy_primitives::B256, reth_storage_api::errors::ProviderError> {
        Ok(reth_storage_api::BlockHashReader::block_hash(self.provider, number)?
            .unwrap_or_default())
    }
}

/// Convert a recovered transaction to a pipeline input.
///
/// Uses `ToTxEnv` trait which properly handles all transaction types
/// (Legacy, EIP-2930, EIP-1559, EIP-4844, EIP-7702) including
/// access_list, max_fee_per_blob_gas, blob_hashes, authorization_list, etc.
fn tx_to_pipeline_input(tx: &Recovered<TransactionSigned>, idx: usize) -> PipelineTxInput {
    let tx_env: revm::context::TxEnv = tx.to_tx_env();
    PipelineTxInput { sender: tx.signer(), tx_env, original_index: idx, pre_crw_sets: None }
}

/// Parallel payload builder that uses `ParallelExecutionPipeline`.
///
/// Implements the `PayloadBuilder` trait so it can be plugged into reth's
/// payload service via `BasicPayloadServiceBuilder`.
#[derive(Debug)]
pub(crate) struct ParallelPayloadBuilder<Pool, Client, EvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    pipeline: parking_lot::Mutex<ParallelExecutionPipeline>,
    /// QMDB store for committing state and computing correct state root during payload building.
    qmdb_store: Option<Arc<xlayer_qmdb_provider::QmdbStore>>,
}

impl<Pool: Clone, Client: Clone, EvmConfig: Clone> Clone
    for ParallelPayloadBuilder<Pool, Client, EvmConfig>
{
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            pool: self.pool.clone(),
            evm_config: self.evm_config.clone(),
            builder_config: self.builder_config.clone(),
            // Each clone gets its own pipeline (thread pools are cheap to create)
            pipeline: parking_lot::Mutex::new(ParallelExecutionPipeline::with_config(16, 12, 64)),
            qmdb_store: self.qmdb_store.clone(),
        }
    }
}

impl<Pool, Client, EvmConfig> ParallelPayloadBuilder<Pool, Client, EvmConfig> {
    /// Create a new parallel payload builder.
    pub(crate) fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            pipeline: parking_lot::Mutex::new(ParallelExecutionPipeline::with_config(16, 12, 64)),
            qmdb_store: None,
        }
    }

    /// Set the QMDB store for computing correct state roots during payload building.
    pub(crate) fn with_qmdb_store(mut self, store: Arc<xlayer_qmdb_provider::QmdbStore>) -> Self {
        self.qmdb_store = Some(store);
        self
    }
}

impl<Pool, Client, EvmConfig> PayloadBuilder for ParallelPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        parallel_ethereum_payload(
            &self.evm_config,
            &self.client,
            &self.pool,
            &self.builder_config,
            &self.pipeline,
            self.qmdb_store.as_ref(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        // Empty block: no transactions to parallelize, use the standard path.
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);
        self.try_build(args)?.into_payload().ok_or(PayloadBuilderError::MissingPayload)
    }
}

/// Build an Ethereum payload using true parallel transaction execution.
///
/// This is the core function. It replaces the sequential execution loop in
/// `default_ethereum_payload` with our `ParallelExecutionPipeline`.
#[allow(clippy::too_many_arguments)]
fn parallel_ethereum_payload<EvmConfig, Client, Pool, F>(
    evm_config: &EvmConfig,
    client: &Client,
    pool: &Pool,
    builder_config: &EthereumBuilderConfig,
    pipeline: &parking_lot::Mutex<ParallelExecutionPipeline>,
    qmdb_store: Option<&Arc<xlayer_qmdb_provider::QmdbStore>>,
    args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    best_txs: F,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
    let BuildArguments { mut cached_reads, config, cancel, best_payload } = args;
    let PayloadConfig { parent_header, attributes } = config;

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(state_provider.as_ref());
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

    let chain_spec = client.chain_spec();

    debug!(target: "payload_builder::parallel", id=%attributes.id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload (parallel)");

    // Block timing metrics (same system as sequential builder)
    let prom_metrics = BlockTimingPrometheusMetrics::default();
    let mut timing_ctx =
        BlockTimingContext::new_empty_with_prometheus(alloy_primitives::B256::ZERO, prom_metrics);

    // -----------------------------------------------------------------------
    // Phase 1: Pre-execution changes via standard builder
    // -----------------------------------------------------------------------
    let gas_limit = builder_config.gas_limit(parent_header.gas_limit);
    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit,
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: Some(attributes.withdrawals().clone()),
                extra_data: builder_config.extra_data.clone(),
            },
        )
        .map_err(PayloadBuilderError::other)?;

    // Derive block env from attributes and parent header
    let block_gas_limit: u64 = gas_limit;
    let base_fee: u64 = parent_header
        .next_block_base_fee(chain_spec.base_fee_params_at_timestamp(attributes.timestamp()))
        .unwrap_or_default();
    let block_number: u64 = parent_header.number + 1;
    let block_timestamp: u64 = attributes.timestamp();
    let beneficiary = attributes.suggested_fee_recipient();
    let difficulty = U256::ZERO; // post-merge
    let prevrandao = Some(attributes.prev_randao());
    let blob_gasprice: Option<u64> = parent_header
        .maybe_next_block_excess_blob_gas(
            chain_spec.blob_params_at_timestamp(attributes.timestamp()),
        )
        .map(|excess| alloy_eips::eip4844::calc_blob_gasprice(excess) as u64);

    {
        let _guard = timing_ctx.time_apply_pre_execution_changes();
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder::parallel", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;
    }

    // Drop builder to release &mut db borrow.
    // Pre-execution state changes (beacon root, system calls) are already applied to db.
    drop(builder);

    // -----------------------------------------------------------------------
    // Phase 2: Collect transactions from pool
    // -----------------------------------------------------------------------
    let pool_status = pool.pool_size();
    info!(
        target: "payload_builder::parallel",
        pending = pool_status.pending,
        queued = pool_status.queued,
        "pool status before tx collection"
    );

    let collect_start = std::time::Instant::now();
    let _select_guard = timing_ctx.time_select_mempool_transactions();
    let mut best_txs = best_txs(BestTransactionsAttributes::new(base_fee, blob_gasprice));

    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp());
    let protocol_max_blob_count =
        blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);
    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp());
    let withdrawals_rlp_length = attributes.withdrawals().length();

    let mut candidates: Vec<(
        Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>,
        Recovered<TransactionSigned>,
    )> = Vec::new();
    let mut blob_sidecars = BlobSidecars::Empty;
    let mut block_blob_count = 0u64;
    let mut block_transactions_rlp_length = 0usize;

    // Collect all available transactions from pool.
    // The BestTransactions iterator is snapshot-based (no pool lock held),
    // with a broadcast channel for newly arrived txs (try_recv on each next()).
    while let Some(pool_tx) = best_txs.next() {
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        let tx = pool_tx.to_consensus();

        // Individual gas limit check (single tx can't exceed block limit)
        if pool_tx.gas_limit() > block_gas_limit {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
            );
            continue;
        }

        // RLP size check (Osaka)
        let tx_rlp_len = tx.inner().length();
        let estimated_block_size =
            block_transactions_rlp_length + tx_rlp_len + withdrawals_rlp_length + 1024;
        if is_osaka && estimated_block_size > MAX_RLP_BLOCK_SIZE {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::OversizedData {
                    size: estimated_block_size,
                    limit: MAX_RLP_BLOCK_SIZE,
                },
            );
            continue;
        }

        // Blob transaction handling
        if let Some(blob_tx) = tx.as_eip4844() {
            let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;
            if block_blob_count + tx_blob_count > max_blob_count {
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::Eip4844(
                        Eip4844PoolTransactionError::TooManyEip4844Blobs {
                            have: block_blob_count + tx_blob_count,
                            permitted: max_blob_count,
                        },
                    ),
                );
                continue;
            }

            let blob_sidecar =
                match pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)? {
                    Some(sidecar) => {
                        if is_osaka {
                            if sidecar.is_eip7594() {
                                sidecar
                            } else {
                                best_txs.mark_invalid(
                                &pool_tx,
                                &InvalidPoolTransactionError::Eip4844(
                                    Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka,
                                ),
                            );
                                continue;
                            }
                        } else if sidecar.is_eip4844() {
                            sidecar
                        } else {
                            best_txs.mark_invalid(
                            &pool_tx,
                            &InvalidPoolTransactionError::Eip4844(
                                Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka,
                            ),
                        );
                            continue;
                        }
                    }
                    None => {
                        best_txs.mark_invalid(
                            &pool_tx,
                            &InvalidPoolTransactionError::Eip4844(
                                Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
                            ),
                        );
                        continue;
                    }
                };

            block_blob_count += tx_blob_count;
            if block_blob_count == max_blob_count {
                best_txs.skip_blobs();
            }
            blob_sidecars.push_sidecar_variant(blob_sidecar.as_ref().clone());
        }

        block_transactions_rlp_length += tx_rlp_len;
        candidates.push((pool_tx, tx));
    }

    let collected_count = candidates.len();
    let collect_elapsed = collect_start.elapsed();
    let estimated_gas: u64 = candidates.iter().map(|(p, _)| p.gas_limit()).sum();
    info!(
        target: "payload_builder::parallel",
        collected = collected_count,
        ?collect_elapsed,
        estimated_gas,
        block_gas_limit,
        cancelled = cancel.is_cancelled(),
        "tx collection done"
    );

    drop(_select_guard);

    // -----------------------------------------------------------------------
    // Phase 3: Parallel execution
    // -----------------------------------------------------------------------
    let _exec_guard = timing_ctx.time_exec_mempool_transactions();
    let pipeline_inputs: Vec<PipelineTxInput> =
        candidates.iter().enumerate().map(|(i, (_, tx))| tx_to_pipeline_input(tx, i)).collect();

    let exec_start = std::time::Instant::now();

    // Build block/cfg env for the pipeline
    let block_env = revm::context::BlockEnv {
        number: U256::from(block_number),
        beneficiary,
        timestamp: U256::from(block_timestamp),
        gas_limit: block_gas_limit,
        basefee: base_fee,
        difficulty,
        prevrandao,
        ..Default::default()
    };
    // Build CfgEnv with proper chain_id, spec_id, and gas params
    let spec = spec_by_timestamp_and_block_number(
        chain_spec.as_ref(),
        attributes.timestamp(),
        block_number,
    );
    let mut cfg_env = revm::context::CfgEnv::new()
        .with_chain_id(chain_spec.chain().id())
        .with_spec_and_mainnet_gas_params(spec);

    if let Some(ref bp) = blob_params {
        cfg_env.set_max_blobs_per_tx(bp.max_blobs_per_tx);
    }

    // Use QmdbDirectDbRef when QMDB is available — bypasses StateProvider layers.
    // Matches fafo's direct QMDB read path for account/storage.
    let pipeline_result = if let Some(store) = qmdb_store {
        let db = QmdbDirectDbRef { store: store.as_ref(), provider: state_provider.as_ref() };
        pipeline.lock().execute_block(pipeline_inputs, &db, &block_env, &cfg_env)
    } else {
        let db = SimDatabaseRef { provider: state_provider.as_ref() };
        pipeline.lock().execute_block(pipeline_inputs, &db, &block_env, &cfg_env)
    };

    let exec_elapsed = exec_start.elapsed();

    // Drop the pool iterator — we're done collecting transactions.
    drop(best_txs);

    // -----------------------------------------------------------------------
    // Phase 4: Build receipts + merge state + single commit
    // -----------------------------------------------------------------------
    // Previous approach: N preloads + N commits + N state.clone() = ~10ms overhead
    // New approach: build receipts while merging EvmStates into one, then
    // single preload + single commit. Eliminates per-tx cloning and repeated
    // HashMap insertions.

    let commit_start = std::time::Instant::now();

    let mut cumulative_gas_used = 0u64;
    let mut total_fees = U256::ZERO;
    let mut blob_gas_used = 0u64;

    let tx_result_count = pipeline_result.tx_results.len();
    let mut receipts: Vec<Receipt> = Vec::with_capacity(tx_result_count);
    let mut executed_senders: Vec<alloy_primitives::Address> = Vec::with_capacity(candidates.len());
    let mut executed_txs: Vec<TransactionSigned> = Vec::with_capacity(candidates.len());

    // Merge all tx EvmStates into one combined state (no cloning — move semantics).
    // Later txs' writes override earlier txs' writes for the same address/slot.
    let mut merged_state: revm::state::EvmState = Default::default();

    for tx_result in pipeline_result.tx_results {
        let idx = tx_result.original_index;
        let (_, tx) = &candidates[idx];

        cumulative_gas_used += tx_result.gas_used;

        // Calculate miner fee
        let miner_fee = tx
            .effective_tip_per_gas(base_fee)
            .expect("fee is always valid; transaction was validated");
        total_fees += U256::from(miner_fee) * U256::from(tx_result.gas_used);

        // Track blob gas
        if let Some(blob_tx) = tx.as_eip4844() {
            let blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;
            let gas_per_blob = alloy_eips::eip4844::DATA_GAS_PER_BLOB;
            blob_gas_used += blob_count * gas_per_blob;
        }

        // Build receipt
        let logs = match &tx_result.result {
            revm::context::result::ExecutionResult::Success { logs, .. } => logs.clone(),
            _ => Vec::new(),
        };

        receipts.push(Receipt {
            tx_type: tx.tx_type(),
            success: tx_result.success,
            cumulative_gas_used,
            logs,
        });

        // Collect transaction + sender for block body
        let (inner_tx, signer) = tx.clone().into_parts();
        executed_txs.push(inner_tx);
        executed_senders.push(signer);

        // Merge this tx's state into combined state (moved, no clone)
        for (addr, account) in tx_result.state {
            match merged_state.entry(addr) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let existing = entry.get_mut();
                    existing.info = account.info;
                    existing.status = account.status;
                    for (slot, val) in account.storage {
                        existing.storage.insert(slot, val);
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(account);
                }
            }
        }
    }

    // Single preload: load unique addresses into State<DB> cache
    {
        use revm::Database;
        for addr in merged_state.keys() {
            let _ = db.basic(*addr);
        }
    }

    // Single commit: all state changes at once (no clone needed)
    db.commit(merged_state);

    let commit_elapsed = commit_start.elapsed();

    info!(
        target: "payload_builder::parallel",
        tx_count = tx_result_count,
        gas_used = cumulative_gas_used,
        ?exec_elapsed,
        ?commit_elapsed,
        "parallel payload: pipeline done"
    );

    // Check if we have a better block
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        return Ok(BuildOutcome::Aborted { fees: total_fees, cached_reads });
    }

    drop(_exec_guard);

    let finalize_start = std::time::Instant::now();

    // Merge all transitions (pre-execution + parallel results) into BundleState
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_state = db.take_bundle();

    // State root: QMDB pipeline root (near-instant read from cache).
    // QMDB writes happen asynchronously via on_canonical_commit → flusher thread.
    let state_root;
    {
        let _root_guard = timing_ctx.time_calc_state_root();
        state_root = if let Some(store) = qmdb_store {
            store.last_flushed_root()
        } else {
            state_provider
                .state_root_with_updates(HashedPostState::default())
                .map_err(|e| PayloadBuilderError::Internal(e.into()))?
                .0
        };
    }

    let hashed_state = HashedPostState::default();
    let trie_updates = TrieUpdates::default();

    // Build execution result
    // Note: post-execution system call requests (EIP-7002, EIP-7251) are not computed here
    // because we bypass reth's sequential builder. For L1 blocks with validator operations
    // this would need to be addressed; for benchmarking/L2 this is fine.
    let requests: alloy_eips::eip7685::Requests = Default::default();
    let execution_result = BlockExecutionResult {
        receipts: receipts.clone(),
        requests: requests.clone(),
        gas_used: cumulative_gas_used,
        blob_gas_used,
    };

    // Assemble block header
    let timestamp = block_timestamp;
    let transactions_root = proofs::calculate_transaction_root(&executed_txs);
    let receipts_root = proofs::calculate_receipt_root(
        &receipts.iter().map(|r| r.with_bloom_ref()).collect::<Vec<_>>(),
    );
    let logs_bloom_value = logs_bloom(receipts.iter().flat_map(|r| &r.logs));

    let withdrawals = chain_spec
        .is_shanghai_active_at_timestamp(timestamp)
        .then(|| attributes.withdrawals().clone());
    let withdrawals_root = withdrawals.as_deref().map(|w| proofs::calculate_withdrawals_root(w));
    let requests_hash =
        chain_spec.is_prague_active_at_timestamp(timestamp).then(|| requests.requests_hash());

    let mut excess_blob_gas = None;
    let mut block_blob_gas_used = None;
    if chain_spec.is_cancun_active_at_timestamp(timestamp) {
        block_blob_gas_used = Some(blob_gas_used);
        excess_blob_gas = if chain_spec.is_cancun_active_at_timestamp(parent_header.timestamp) {
            parent_header
                .maybe_next_block_excess_blob_gas(chain_spec.blob_params_at_timestamp(timestamp))
        } else {
            Some(
                alloy_eips::eip7840::BlobParams::cancun().next_block_excess_blob_gas_osaka(0, 0, 0),
            )
        };
    }

    let header = Header {
        parent_hash: parent_header.hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary,
        state_root,
        transactions_root,
        receipts_root,
        withdrawals_root,
        logs_bloom: logs_bloom_value,
        timestamp,
        mix_hash: prevrandao.unwrap_or_default(),
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(base_fee),
        number: block_number,
        gas_limit: block_gas_limit,
        difficulty,
        gas_used: cumulative_gas_used,
        extra_data: builder_config.extra_data.clone(),
        parent_beacon_block_root: attributes.parent_beacon_block_root(),
        blob_gas_used: block_blob_gas_used,
        excess_blob_gas,
        requests_hash,
    };

    let block = Block {
        header,
        body: BlockBody {
            transactions: executed_txs.clone(),
            ommers: Default::default(),
            withdrawals,
        },
    };

    let recovered_block = Arc::new(RecoveredBlock::new_unhashed(block, executed_senders));
    let sealed_block = Arc::new(recovered_block.sealed_block().clone());

    debug!(target: "payload_builder::parallel", id=%attributes.id, sealed_block_header = ?sealed_block.sealed_header(), "sealed built block (parallel)");

    if is_osaka && sealed_block.rlp_length() > MAX_RLP_BLOCK_SIZE {
        return Err(PayloadBuilderError::other(reth_errors::ConsensusError::BlockTooLarge {
            rlp_length: sealed_block.rlp_length(),
            max_rlp_length: MAX_RLP_BLOCK_SIZE,
        }));
    }

    // Build executed block for InsertExecutedBlock fast path
    let executed_block = {
        use either::Either;
        use reth_evm::execute::BlockExecutionOutput;
        BuiltPayloadExecutedBlock {
            recovered_block,
            execution_output: Arc::new(BlockExecutionOutput {
                result: execution_result,
                state: bundle_state,
            }),
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_updates)),
        }
    };

    let finalize_elapsed = finalize_start.elapsed();
    let total_elapsed = exec_start.elapsed();
    let tps = if total_elapsed.as_micros() > 0 {
        (tx_result_count as f64 / total_elapsed.as_secs_f64()) as u64
    } else {
        0
    };
    let gas_throughput = if total_elapsed.as_micros() > 0 {
        cumulative_gas_used as f64 / total_elapsed.as_secs_f64() / 1e9
    } else {
        0.0
    };

    info!(
        target: "payload_builder::parallel",
        number = block_number,
        txs = tx_result_count,
        gas_used = cumulative_gas_used,
        ?total_elapsed,
        ?exec_elapsed,
        ?commit_elapsed,
        ?finalize_elapsed,
        tps,
        gas_gps = format_args!("{gas_throughput:.2}"),
        "parallel payload built"
    );

    // Store build timing with actual block hash
    timing_ctx.set_block_hash(sealed_block.hash());
    timing_ctx.update_totals();
    timing_ctx.store();

    let payload = EthBuiltPayload::new(
        attributes.id,
        sealed_block,
        total_fees,
        chain_spec.is_prague_active_at_timestamp(timestamp).then_some(requests.clone()),
    )
    .with_sidecars(blob_sidecars)
    .with_executed_block(executed_block);

    Ok(BuildOutcome::Better { payload, cached_reads })
}
