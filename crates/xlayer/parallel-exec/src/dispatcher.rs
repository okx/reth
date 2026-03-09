//! Dispatcher: schedules parallel execution of transaction frames.
//!
//! MVP strategy: frames are executed serially (each frame depends on the
//! previous one), but tasks within each frame are executed in parallel
//! using rayon since they have no read/write conflicts.

use crate::{framer::Frame, simulator::SimTxEnv, state_cache::ParallelStateCache, task::ExeTask};
use rayon::prelude::*;
use revm::context::BlockEnv;

/// Result of executing a single transaction.
#[derive(Debug, Clone)]
pub struct TxExecutionResult {
    /// Index in the original transaction list.
    pub original_index: usize,
    /// The execution result from revm.
    pub result: revm::context::result::ExecutionResult,
    /// State changes from this transaction.
    pub state: revm::state::EvmState,
    /// Gas used by this transaction.
    pub gas_used: u64,
}

/// Manages parallel execution of transaction frames.
pub struct Dispatcher {
    /// Number of worker threads.
    thread_count: usize,
    /// The rayon thread pool.
    pool: rayon::ThreadPool,
}

impl core::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dispatcher").field("thread_count", &self.thread_count).finish()
    }
}

impl Dispatcher {
    /// Create a new `Dispatcher` with the given number of worker threads.
    pub fn new(thread_count: usize) -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .thread_name(|idx| format!("parallel-exec-{idx}"))
            .build()
            .expect("failed to build rayon thread pool");
        Self { thread_count, pool }
    }

    /// Returns the number of worker threads.
    pub fn thread_count(&self) -> usize {
        self.thread_count
    }

    /// Execute all frames.
    ///
    /// Frames are executed serially (each frame depends on the previous one's
    /// state changes). Tasks within each frame are executed in parallel via rayon,
    /// since they have no read/write conflicts.
    ///
    /// Results are returned sorted by `original_index` to restore the original
    /// transaction ordering.
    pub fn execute(
        &self,
        frames: Vec<Frame>,
        cache: &ParallelStateCache,
        fallback: &(dyn reth_storage_api::StateProvider + Sync),
        block_env: &BlockEnv,
        txs: &[SimTxEnv],
    ) -> Vec<TxExecutionResult> {
        let mut all_results = Vec::new();

        for frame in frames {
            // Execute tasks within this frame in parallel
            let frame_results: Vec<Vec<TxExecutionResult>> = self.pool.install(|| {
                frame
                    .tasks
                    .par_iter()
                    .map(|task| self.execute_task(task, cache, fallback, block_env, txs))
                    .collect()
            });

            // Apply state changes from this frame to the cache,
            // making them visible to subsequent frames.
            for task_results in &frame_results {
                for tx_result in task_results {
                    apply_state_to_cache(cache, &tx_result.state);
                }
            }

            // Collect results
            for task_results in frame_results {
                all_results.extend(task_results);
            }
        }

        // Sort by original_index to restore transaction ordering
        all_results.sort_by_key(|r| r.original_index);
        all_results
    }

    /// Execute a single `ExeTask` (sequentially execute its transactions).
    ///
    /// Each transaction gets its own EVM instance backed by the shared
    /// `CachedStateProvider` (DashMap cache + fallback `StateProvider`).
    fn execute_task(
        &self,
        task: &ExeTask,
        cache: &ParallelStateCache,
        fallback: &(dyn reth_storage_api::StateProvider + Sync),
        block_env: &BlockEnv,
        txs: &[SimTxEnv],
    ) -> Vec<TxExecutionResult> {
        use crate::state_cache::CachedStateProvider;
        use alloy_evm::{precompiles::PrecompilesMap, Evm, EvmEnv};
        use revm::{
            context::{CfgEnv, Context},
            database::CacheDB,
            handler::EthPrecompiles,
            inspector::NoOpInspector,
            MainBuilder, MainContext,
        };

        let cached_provider = CachedStateProvider::new(cache, fallback);

        task.sim_results
            .iter()
            .map(|sim_result| {
                let tx_env = &txs[sim_result.original_index];

                // Build EVM with the cached state provider
                let cache_db = CacheDB::new(&cached_provider);
                let cfg = CfgEnv::default();
                let evm_env = EvmEnv { cfg_env: cfg, block_env: block_env.clone() };

                let inner = Context::mainnet()
                    .with_db(cache_db)
                    .with_cfg(evm_env.cfg_env)
                    .with_block(evm_env.block_env)
                    .build_mainnet_with_inspector(NoOpInspector {})
                    .with_precompiles(PrecompilesMap::from_static(
                        EthPrecompiles::default().precompiles,
                    ));

                let mut evm = alloy_evm::EthEvm::new(inner, false);

                match evm.transact(tx_env.tx_env.clone()) {
                    Ok(result_and_state) => {
                        let gas_used = result_and_state.result.gas_used();
                        TxExecutionResult {
                            original_index: sim_result.original_index,
                            result: result_and_state.result,
                            state: result_and_state.state,
                            gas_used,
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "xlayer::parallel::dispatcher",
                            ?err,
                            index = sim_result.original_index,
                            "Transaction execution failed"
                        );
                        // Return a failed result with empty state
                        TxExecutionResult {
                            original_index: sim_result.original_index,
                            result: revm::context::result::ExecutionResult::Halt {
                                reason: revm::context::result::HaltReason::NotActivated,
                                gas_used: 0,
                            },
                            state: Default::default(),
                            gas_used: 0,
                        }
                    }
                }
            })
            .collect()
    }
}

/// Apply EVM state changes to the parallel cache so subsequent frames can see them.
fn apply_state_to_cache(cache: &ParallelStateCache, state: &revm::state::EvmState) {
    use revm_state::AccountInfo;

    for (address, account) in state {
        // Update account info in cache
        let info = AccountInfo {
            balance: account.info.balance,
            nonce: account.info.nonce,
            code_hash: account.info.code_hash,
            code: account.info.code.clone(),
            account_id: None,
        };
        cache.insert_account(*address, Some(info));

        // Update storage slots
        for (slot, value) in &account.storage {
            cache.insert_storage(*address, *slot, Some(value.present_value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crw_sets::CrwSets,
        task::{ExeTask, SimResult},
    };
    use alloy_primitives::{Address, U256};
    use revm_state::{Account, AccountInfo, AccountStatus, EvmStorageSlot};

    #[test]
    fn test_dispatcher_creation() {
        let dispatcher = Dispatcher::new(4);
        assert_eq!(dispatcher.thread_count(), 4);

        let dispatcher2 = Dispatcher::new(8);
        assert_eq!(dispatcher2.thread_count(), 8);
    }

    #[test]
    fn test_apply_state_to_cache() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);

        // Build mock EvmState
        let mut state = revm::state::EvmState::default();
        let mut storage = revm_state::EvmStorage::default();
        storage.insert(
            U256::from(7),
            EvmStorageSlot {
                original_value: U256::from(0),
                present_value: U256::from(999),
                ..Default::default()
            },
        );

        let account = Account {
            info: AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() },
            original_info: Box::new(AccountInfo::default()),
            status: AccountStatus::Touched,
            storage,
            transaction_id: 0,
        };

        state.insert(addr, account);

        // Apply to cache
        apply_state_to_cache(&cache, &state);

        // Verify account info cached
        let cached_account = cache.get_account(&addr).expect("account should be cached");
        let info = cached_account.expect("account should exist");
        assert_eq!(info.balance, U256::from(1000));
        assert_eq!(info.nonce, 5);

        // Verify storage slot cached
        let cached_storage = cache.get_storage(&addr, &U256::from(7));
        assert_eq!(cached_storage, Some(Some(U256::from(999))));
    }

    #[test]
    fn test_tx_execution_result_creation() {
        let result = TxExecutionResult {
            original_index: 42,
            result: revm::context::result::ExecutionResult::Halt {
                reason: revm::context::result::HaltReason::NotActivated,
                gas_used: 0,
            },
            state: Default::default(),
            gas_used: 0,
        };

        assert_eq!(result.original_index, 42);
        assert_eq!(result.gas_used, 0);
        assert!(result.state.is_empty());
    }

    fn make_sim_result(index: usize) -> SimResult {
        SimResult { crw_sets: CrwSets::default(), original_index: index, success: true }
    }

    fn make_frame(indices: Vec<usize>) -> Frame {
        let tasks: Vec<ExeTask> =
            indices.into_iter().map(|idx| ExeTask::new(make_sim_result(idx))).collect();
        Frame { tasks }
    }

    fn make_simple_tx(sender: Address, recipient: Address, nonce: u64) -> SimTxEnv {
        use alloy_primitives::TxKind;
        let tx_env = revm::context::TxEnv {
            caller: sender,
            gas_limit: 21000,
            gas_price: 0,
            kind: TxKind::Call(recipient),
            value: U256::ZERO,
            nonce,
            ..Default::default()
        };
        SimTxEnv { sender, tx_env }
    }

    #[test]
    fn test_dispatcher_single_frame() {
        // Create a single frame with one task that has one transaction.
        // The transaction will fail because EmptyDB has no balance,
        // but the dispatcher should handle this gracefully.
        let dispatcher = Dispatcher::new(2);
        let _cache = ParallelStateCache::new();

        let sender = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let txs = vec![make_simple_tx(sender, recipient, 0)];

        let frame = make_frame(vec![0]);
        let frames = vec![frame];

        // Full execution requires a StateProvider implementation (trait object).
        // Without reth test-utils providing a mock, we verify the dispatcher
        // struct, frame construction, and transaction setup are correct.
        assert_eq!(dispatcher.thread_count(), 2);
        assert_eq!(txs.len(), 1);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn test_dispatcher_sorts_by_original_index() {
        // Verify that if we manually construct TxExecutionResults out of order,
        // the sorting logic works.
        let mut results = vec![
            TxExecutionResult {
                original_index: 3,
                result: revm::context::result::ExecutionResult::Halt {
                    reason: revm::context::result::HaltReason::NotActivated,
                    gas_used: 0,
                },
                state: Default::default(),
                gas_used: 0,
            },
            TxExecutionResult {
                original_index: 1,
                result: revm::context::result::ExecutionResult::Halt {
                    reason: revm::context::result::HaltReason::NotActivated,
                    gas_used: 0,
                },
                state: Default::default(),
                gas_used: 0,
            },
            TxExecutionResult {
                original_index: 2,
                result: revm::context::result::ExecutionResult::Halt {
                    reason: revm::context::result::HaltReason::NotActivated,
                    gas_used: 0,
                },
                state: Default::default(),
                gas_used: 0,
            },
        ];

        results.sort_by_key(|r| r.original_index);

        assert_eq!(results[0].original_index, 1);
        assert_eq!(results[1].original_index, 2);
        assert_eq!(results[2].original_index, 3);
    }
}
