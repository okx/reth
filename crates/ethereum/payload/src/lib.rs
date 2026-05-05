//! A basic Ethereum payload builder implementation.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(clippy::useless_let_if_seq)]

use alloy_consensus::Transaction;
use alloy_primitives::U256;
use alloy_rlp::Encodable;
use reth_basic_payload_builder::{
    is_better_payload, BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::{BlockExecutionError, BlockValidationError, ConsensusError};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    execute::{BlockBuilder, BlockBuilderOutcome},
    ConfigureEvm, Evm, NextBlockEnvAttributes,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_node_metrics::block_timing::{BlockTimingContext, BlockTimingPrometheusMetrics};
use reth_payload_builder::{BlobSidecars, EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadBuilderAttributes};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};
use revm::context_interface::Block as _;
use revm_database::states::bundle_state::BundleRetention;
use std::sync::Arc;
use tracing::{debug, trace, warn};

mod parallel;
use parallel::{tx_to_sim_env, SimDatabaseRef};
use xlayer_parallel_exec::simulator::Simulator;

mod config;
pub use config::*;

pub mod validator;
pub use validator::EthereumExecutionPayloadValidator;

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Ethereum payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration.
    builder_config: EthereumBuilderConfig,
}

impl<Pool, Client, EvmConfig> EthereumPayloadBuilder<Pool, Client, EvmConfig> {
    /// `EthereumPayloadBuilder` constructor.
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
    ) -> Self {
        Self { client, pool, evm_config, builder_config }
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> PayloadBuilder for EthereumPayloadBuilder<Pool, Client, EvmConfig>
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
        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        if self.builder_config.await_payload_on_missing {
            MissingPayloadBehaviour::AwaitInProgress
        } else {
            MissingPayloadBehaviour::RaceEmptyPayload
        }
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

/// Constructs an Ethereum transaction payload using the best transactions from the pool.
///
/// Given build arguments including an Ethereum client, transaction pool,
/// and configuration, this function creates a transaction payload. Returns
/// a result indicating success with the payload or an error in case of failure.
#[inline]
pub fn default_ethereum_payload<EvmConfig, Client, Pool, F>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
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

    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: Some(attributes.withdrawals().clone()),
                extra_data: builder_config.extra_data,
                slot_number: None,
            },
        )
        .map_err(PayloadBuilderError::other)?;

    let chain_spec = client.chain_spec();

    debug!(target: "payload_builder", id=%attributes.id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let mut cumulative_gas_used = 0;
    let block_gas_limit: u64 = builder.evm_mut().block().gas_limit();
    let base_fee = builder.evm_mut().block().basefee();

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        builder.evm_mut().block().blob_gasprice().map(|gasprice| gasprice as u64),
    ));
    let mut total_fees = U256::ZERO;

    // Initialize build timing context (block hash unknown until block is sealed)
    let prom_metrics = BlockTimingPrometheusMetrics::default();
    let mut timing_ctx =
        BlockTimingContext::new_empty_with_prometheus(alloy_primitives::B256::ZERO, prom_metrics);

    {
        let _guard = timing_ctx.time_apply_pre_execution_changes();
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;
    }

    // initialize empty blob sidecars at first. If cancun is active then this will be populated by
    // blob sidecars if any.
    let mut blob_sidecars = BlobSidecars::Empty;

    let mut block_blob_count = 0;
    let mut block_transactions_rlp_length = 0;

    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp);
    let protocol_max_blob_count =
        blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_else(Default::default);

    // Apply user-configured blob limit (EIP-7872)
    // Per EIP-7872: if the minimum is zero, set it to one
    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);

    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp);

    let withdrawals_rlp_length = attributes.withdrawals().length();

    {
        let _exec_guard = timing_ctx.time_exec_mempool_transactions();

        if builder_config.parallel_exec {
            // --- Parallel path ---
            // Collect all candidates, launch background simulation, execute sequentially.

            // 1. Collect candidates from pool iterator
            let mut candidates: Vec<(
                Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>,
                alloy_consensus::transaction::Recovered<TransactionSigned>,
            )> = Vec::new();
            while let Some(pool_tx) = best_txs.next() {
                let tx = pool_tx.to_consensus();
                candidates.push((pool_tx, tx));
            }

            if !candidates.is_empty() {
                // 2. Build SimTxEnvs for background simulation (skip blob txs)
                let sim_txs: Vec<_> = candidates
                    .iter()
                    .filter(|(_, tx)| !tx.is_eip4844())
                    .map(|(_, tx)| tx_to_sim_env(tx))
                    .collect();

                let sim_db = SimDatabaseRef { provider: state_provider.as_ref() };
                let block_env = revm::context::BlockEnv {
                    number: builder.evm_mut().block().number().saturating_to(),
                    beneficiary: builder.evm_mut().block().beneficiary(),
                    timestamp: builder.evm_mut().block().timestamp().saturating_to(),
                    gas_limit: builder.evm_mut().block().gas_limit(),
                    basefee: builder.evm_mut().block().basefee(),
                    ..Default::default()
                };

                let sim_start = std::time::Instant::now();

                // 3. Scoped thread: simulation in background, execution on main thread
                let exec_result: Result<bool, PayloadBuilderError> = std::thread::scope(|scope| {
                    let sim_handle = scope.spawn(|| {
                        let simulator = Simulator::new();
                        simulator.simulate(&sim_txs, &sim_db, &block_env)
                    });

                    for (pool_tx, tx) in candidates {
                        if cancel.is_cancelled() {
                            return Ok(true);
                        }

                        if cumulative_gas_used + tx.gas_limit() > block_gas_limit {
                            best_txs.mark_invalid(
                                &pool_tx,
                                &InvalidPoolTransactionError::ExceedsGasLimit(
                                    tx.gas_limit(),
                                    block_gas_limit,
                                ),
                            );
                            continue;
                        }

                        let tx_rlp_len = tx.inner().length();
                        let estimated_block_size_with_tx = block_transactions_rlp_length +
                            tx_rlp_len +
                            withdrawals_rlp_length +
                            1024;
                        if is_osaka && estimated_block_size_with_tx > MAX_RLP_BLOCK_SIZE {
                            best_txs.mark_invalid(
                                &pool_tx,
                                &InvalidPoolTransactionError::OversizedData {
                                    size: estimated_block_size_with_tx,
                                    limit: MAX_RLP_BLOCK_SIZE,
                                },
                            );
                            continue;
                        }

                        // Blob transaction handling
                        let mut blob_tx_sidecar = None;
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

                            let blob_sidecar_result = 'sidecar: {
                                let Some(sidecar) = pool
                                    .get_blob(*tx.hash())
                                    .map_err(PayloadBuilderError::other)?
                                else {
                                    break 'sidecar Err(
                                        Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
                                    )
                                };
                                if is_osaka {
                                    if sidecar.is_eip7594() {
                                        Ok(sidecar)
                                    } else {
                                        Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                                    }
                                } else if sidecar.is_eip4844() {
                                    Ok(sidecar)
                                } else {
                                    Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                                }
                            };
                            blob_tx_sidecar = match blob_sidecar_result {
                                Ok(sidecar) => Some(sidecar),
                                Err(error) => {
                                    best_txs.mark_invalid(
                                        &pool_tx,
                                        &InvalidPoolTransactionError::Eip4844(error),
                                    );
                                    continue;
                                }
                            };
                        }

                        let gas_used = match builder.execute_transaction(tx.clone()) {
                            Ok(gas_used) => gas_used,
                            Err(BlockExecutionError::Validation(
                                BlockValidationError::InvalidTx { error, .. },
                            )) => {
                                if error.is_nonce_too_low() {
                                    trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                                } else {
                                    trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                                    best_txs.mark_invalid(
                                        &pool_tx,
                                        &InvalidPoolTransactionError::Consensus(
                                            InvalidTransactionError::TxTypeNotSupported,
                                        ),
                                    );
                                }
                                continue;
                            }
                            Err(err) => return Err(PayloadBuilderError::evm(err)),
                        };

                        if let Some(blob_tx) = tx.as_eip4844() {
                            block_blob_count += blob_tx.tx().blob_versioned_hashes.len() as u64;
                            if block_blob_count == max_blob_count {
                                best_txs.skip_blobs();
                            }
                        }

                        block_transactions_rlp_length += tx_rlp_len;
                        let miner_fee = tx
                            .effective_tip_per_gas(base_fee)
                            .expect("fee is always valid; execution succeeded");
                        let tx_gas_used = gas_used.tx_gas_used();
                        total_fees += U256::from(miner_fee) * U256::from(tx_gas_used);
                        cumulative_gas_used += tx_gas_used;

                        if let Some(sidecar) = blob_tx_sidecar {
                            blob_sidecars.push_sidecar_variant(sidecar.as_ref().clone());
                        }
                    }

                    // Wait for background simulation
                    let sim_results = sim_handle.join().expect("simulation thread panicked");
                    let sim_elapsed = sim_start.elapsed();
                    let total = sim_results.len();
                    let success = sim_results.iter().filter(|r| r.success).count();
                    tracing::info!(
                        target: "payload_builder::parallel",
                        ?sim_elapsed,
                        total,
                        success,
                        failed = total - success,
                        "background simulation complete (CrwSets ready for Phase 2)"
                    );

                    Ok(false)
                });

                match exec_result {
                    Ok(true) => return Ok(BuildOutcome::Cancelled),
                    Err(e) => return Err(e),
                    Ok(false) => {}
                }
            }
        } else {
            // --- Original serial path ---
            while let Some(pool_tx) = best_txs.next() {
                // ensure we still have capacity for this transaction
                if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            block_gas_limit,
                        ),
                    );
                    continue
                }

                // check if the job was cancelled, if so we can exit early
                if cancel.is_cancelled() {
                    return Ok(BuildOutcome::Cancelled)
                }

                // convert tx to a signed transaction
                let tx = pool_tx.to_consensus();

                let tx_rlp_len = tx.inner().length();

                let estimated_block_size_with_tx =
                    block_transactions_rlp_length + tx_rlp_len + withdrawals_rlp_length + 1024;

                if is_osaka && estimated_block_size_with_tx > MAX_RLP_BLOCK_SIZE {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::OversizedData {
                            size: estimated_block_size_with_tx,
                            limit: MAX_RLP_BLOCK_SIZE,
                        },
                    );
                    continue
                }

                let mut blob_tx_sidecar = None;
                if let Some(blob_tx) = tx.as_eip4844() {
                    let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;

                    if block_blob_count + tx_blob_count > max_blob_count {
                        trace!(target: "payload_builder", tx=?tx.hash(), ?block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
                        best_txs.mark_invalid(
                            &pool_tx,
                            &InvalidPoolTransactionError::Eip4844(
                                Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                    have: block_blob_count + tx_blob_count,
                                    permitted: max_blob_count,
                                },
                            ),
                        );
                        continue
                    }

                    let blob_sidecar_result = 'sidecar: {
                        let Some(sidecar) =
                            pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)?
                        else {
                            break 'sidecar Err(
                                Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
                            )
                        };

                        if is_osaka {
                            if sidecar.is_eip7594() {
                                Ok(sidecar)
                            } else {
                                Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                            }
                        } else if sidecar.is_eip4844() {
                            Ok(sidecar)
                        } else {
                            Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                        }
                    };

                    blob_tx_sidecar = match blob_sidecar_result {
                        Ok(sidecar) => Some(sidecar),
                        Err(error) => {
                            best_txs.mark_invalid(
                                &pool_tx,
                                &InvalidPoolTransactionError::Eip4844(error),
                            );
                            continue
                        }
                    };
                }

                let gas_used = match builder.execute_transaction(tx.clone()) {
                    Ok(gas_used) => gas_used,
                    Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                        error,
                        ..
                    })) => {
                        if error.is_nonce_too_low() {
                            trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                        } else {
                            trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(
                                &pool_tx,
                                &InvalidPoolTransactionError::Consensus(
                                    InvalidTransactionError::TxTypeNotSupported,
                                ),
                            );
                        }
                        continue
                    }
                    Err(err) => return Err(PayloadBuilderError::evm(err)),
                };

                if let Some(blob_tx) = tx.as_eip4844() {
                    block_blob_count += blob_tx.tx().blob_versioned_hashes.len() as u64;
                    if block_blob_count == max_blob_count {
                        best_txs.skip_blobs();
                    }
                }

                block_transactions_rlp_length += tx_rlp_len;

                let miner_fee = tx
                    .effective_tip_per_gas(base_fee)
                    .expect("fee is always valid; execution succeeded");
                let tx_gas_used = gas_used.tx_gas_used();
                total_fees += U256::from(miner_fee) * U256::from(tx_gas_used);
                cumulative_gas_used += tx_gas_used;

                if let Some(sidecar) = blob_tx_sidecar {
                    blob_sidecars.push_sidecar_variant(sidecar.as_ref().clone());
                }
            }
        }
    }

    // check if we have a better block
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        // Release db
        drop(builder);
        // can skip building the block
        return Ok(BuildOutcome::Aborted { fees: total_fees, cached_reads })
    }

    let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } = {
        let _guard = timing_ctx.time_calc_state_root();
        builder.finish(state_provider.as_ref(), None)?
    };

    // Extract BundleState from the execution database for InsertExecutedBlock fast path.
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_state = db.take_bundle();

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp)
        .then_some(execution_result.requests.clone());

    let sealed_block = Arc::new(block.sealed_block().clone());

    // Store build timing with actual block hash
    timing_ctx.set_block_hash(sealed_block.hash());
    timing_ctx.update_totals();
    timing_ctx.store();

    debug!(target: "payload_builder", id=%attributes.id, sealed_block_header = ?sealed_block.sealed_header(), "sealed built block");

    if is_osaka && sealed_block.rlp_length() > MAX_RLP_BLOCK_SIZE {
        return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge {
            rlp_length: sealed_block.rlp_length(),
            max_rlp_length: MAX_RLP_BLOCK_SIZE,
        }));
    }

    // Build the executed block so the engine can skip re-execution.
    let executed_block = {
        use either::Either;
        use reth_evm::execute::BlockExecutionOutput;
        let execution_output =
            Arc::new(BlockExecutionOutput { result: execution_result, state: bundle_state });
        let recovered_block = Arc::new(block);
        BuiltPayloadExecutedBlock {
            recovered_block,
            execution_output,
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_updates)),
        }
    };

    let payload = EthBuiltPayload::new(attributes.id, sealed_block, total_fees, requests)
        .with_sidecars(blob_sidecars)
        .with_executed_block(executed_block);

    Ok(BuildOutcome::Better { payload, cached_reads })
}
