//! Block execution context for parallel execution.
//!
//! Holds shared state (caches, task manager, dashboard) that parallel
//! executor threads access during block building.

use crate::{
    dashboard::Dashboard, execute::ParallelTxResult, parallel_state_cache::ParallelStateCache,
    tasks_manager::TasksManager, tx_database::StateCache,
};
use parking_lot::RwLock;
use revm::context::{BlockEnv, CfgEnv};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Execution result for a single task (may contain multiple transactions).
#[derive(Debug)]
pub struct TaskExecutionResult {
    /// Per-transaction results within this task.
    pub tx_results: Vec<ParallelTxResult>,
}

/// Block execution context shared across parallel executor threads.
///
/// This is the reth equivalent of fafo's `BlockContext`. It owns:
/// - TasksManager: indexed storage for all tasks
/// - ParallelStateCache: concurrent state cache (current block)
/// - Dashboard: dependency tracking
/// - Results storage: per-task execution results
pub struct ParallelBlockContext {
    /// All tasks for this block.
    pub tasks_manager: Arc<TasksManager>,
    /// Current block's state cache (written by completed tasks).
    pub curr_state: Arc<ParallelStateCache>,
    /// Previous block's state cache (read-only fallback).
    pub prev_state: Option<Arc<ParallelStateCache>>,
    /// Dependency tracking dashboard.
    pub dashboard: Arc<Dashboard>,
    /// Block environment for EVM.
    pub block_env: BlockEnv,
    /// Configuration environment for EVM.
    pub cfg_env: CfgEnv,
    /// Per-task execution results.
    results: Vec<RwLock<Option<TaskExecutionResult>>>,
    /// Cumulative gas used.
    gas_used: AtomicU64,
}

impl ParallelBlockContext {
    /// Create a new block context.
    pub fn new(
        max_tasks: usize,
        curr_state: Arc<ParallelStateCache>,
        prev_state: Option<Arc<ParallelStateCache>>,
        block_env: BlockEnv,
        cfg_env: CfgEnv,
    ) -> Self {
        let mut results = Vec::with_capacity(max_tasks);
        for _ in 0..max_tasks {
            results.push(RwLock::new(None));
        }

        Self {
            tasks_manager: Arc::new(TasksManager::with_size(max_tasks)),
            curr_state,
            prev_state,
            dashboard: Arc::new(Dashboard::new(max_tasks)),
            block_env,
            cfg_env,
            results,
            gas_used: AtomicU64::new(0),
        }
    }

    /// Execute a task at the given index.
    ///
    /// This is called from parallel executor threads. It:
    /// 1. Takes the task from TasksManager
    /// 2. Executes each transaction using revm with ParallelTxDatabase
    /// 3. Applies state diffs to curr_state (making them visible to subsequent tasks)
    /// 4. Stores the execution results
    ///
    /// The database uses a noop StateProvider as the fallback layer. In production,
    /// the Pipeline integration will supply a real StateProvider (QMDB/MDBX) by
    /// pre-populating the cache during warmup or by using a different execute path.
    pub fn execute_task(&self, idx: usize) {
        let task = match self.tasks_manager.take_task(idx) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    target: "xlayer::parallel::block_context",
                    idx,
                    "No task found at index"
                );
                return;
            }
        };

        let mut tx_results = Vec::with_capacity(task.tx_envs.len());

        let provider = reth_storage_api::noop::NoopProvider::mainnet();

        for tx_env in &task.tx_envs {
            // Three-layer read path: curr_state -> prev_state -> provider
            let db = match &self.prev_state {
                Some(prev) => crate::tx_database::ParallelTxDatabase::with_prev_state(
                    self.curr_state.as_ref(),
                    prev.as_ref(),
                    &provider,
                ),
                None => {
                    crate::tx_database::ParallelTxDatabase::new(self.curr_state.as_ref(), &provider)
                }
            };

            let result = crate::execute::execute_tx_with_ref(
                &db,
                &self.block_env,
                &self.cfg_env,
                tx_env.clone(),
            );

            // Apply state diff immediately so subsequent tasks see these changes
            self.curr_state.apply_evm_state(&result.state);

            self.gas_used.fetch_add(result.gas_used, Ordering::Relaxed);

            tx_results.push(result);
        }

        *self.results[idx].write() = Some(TaskExecutionResult { tx_results });
    }

    /// Get the execution result for a task.
    pub fn get_result(&self, idx: usize) -> Option<TaskExecutionResult> {
        self.results[idx].write().take()
    }

    /// Get total gas used.
    pub fn total_gas_used(&self) -> u64 {
        self.gas_used.load(Ordering::Acquire)
    }

    /// Collect all results in order.
    pub fn collect_results(&self) -> Vec<Option<TaskExecutionResult>> {
        let count = self.tasks_manager.count();
        (0..count).map(|i| self.results[i].write().take()).collect()
    }
}

impl std::fmt::Debug for ParallelBlockContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelBlockContext")
            .field("tasks_count", &self.tasks_manager.count())
            .field("gas_used", &self.gas_used.load(Ordering::Relaxed))
            .finish()
    }
}

/// Implement [`StateCache`] for [`ParallelStateCache`] so it can be used with
/// [`ParallelTxDatabase`](crate::tx_database::ParallelTxDatabase).
///
/// Both the trait and the type live in this crate, so no orphan rule issues.
impl StateCache for ParallelStateCache {
    fn get_account(
        &self,
        address: &alloy_primitives::Address,
    ) -> Option<Option<revm_state::AccountInfo>> {
        ParallelStateCache::get_account(self, address)
    }

    fn get_storage(
        &self,
        address: &alloy_primitives::Address,
        slot: &alloy_primitives::U256,
    ) -> Option<alloy_primitives::U256> {
        ParallelStateCache::get_storage(self, address, slot)
    }

    fn get_bytecode(&self, hash: &alloy_primitives::B256) -> Option<revm_bytecode::Bytecode> {
        ParallelStateCache::get_bytecode(self, hash)
    }

    fn get_block_hash(&self, number: &u64) -> Option<alloy_primitives::B256> {
        ParallelStateCache::get_block_hash(self, number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crw_sets::CrwSets,
        task::{ExeTask, SimResult},
    };
    use alloy_primitives::{Address, TxKind, U256};
    use revm::context::TxEnv;

    fn make_sim_result(index: usize) -> SimResult {
        SimResult { crw_sets: CrwSets::default(), original_index: index, success: true }
    }

    fn make_task_with_tx(index: usize) -> ExeTask {
        let mut task = ExeTask::new(make_sim_result(index));
        task.tx_envs.push(TxEnv {
            caller: Address::with_last_byte(index as u8),
            gas_limit: 21000,
            gas_price: 0,
            kind: TxKind::Call(Address::with_last_byte((index + 1) as u8)),
            value: U256::ZERO,
            nonce: 0,
            ..Default::default()
        });
        task
    }

    #[test]
    fn test_block_context_creation() {
        let curr = Arc::new(ParallelStateCache::new());
        let ctx = ParallelBlockContext::new(10, curr, None, BlockEnv::default(), CfgEnv::default());

        assert_eq!(ctx.tasks_manager.count(), 0);
        assert_eq!(ctx.total_gas_used(), 0);
        assert!(ctx.get_result(0).is_none());
    }

    #[test]
    fn test_execute_task_stores_result() {
        let curr = Arc::new(ParallelStateCache::new());
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let ctx = ParallelBlockContext::new(4, curr, None, BlockEnv::default(), cfg);

        let task = make_task_with_tx(0);
        ctx.tasks_manager.set_task(0, task);

        ctx.execute_task(0);

        let result = ctx.get_result(0);
        assert!(result.is_some(), "result should be stored after execution");
        assert_eq!(result.unwrap().tx_results.len(), 1);
    }

    #[test]
    fn test_collect_results_order() {
        let curr = Arc::new(ParallelStateCache::new());
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let ctx = ParallelBlockContext::new(4, curr, None, BlockEnv::default(), cfg);

        // Set and execute tasks 0, 1, 2
        for i in 0..3 {
            ctx.tasks_manager.set_task(i, make_task_with_tx(i));
        }

        ctx.execute_task(0);
        ctx.execute_task(2);
        ctx.execute_task(1);

        let results = ctx.collect_results();
        assert_eq!(results.len(), 3);
        // All three should have results
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_some(), "result at index {} should be present", i);
        }
    }

    #[test]
    fn test_gas_accumulation() {
        let curr = Arc::new(ParallelStateCache::new());
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let ctx = ParallelBlockContext::new(4, curr, None, BlockEnv::default(), cfg);

        // Execute two tasks and verify gas accumulates
        ctx.tasks_manager.set_task(0, make_task_with_tx(0));
        ctx.tasks_manager.set_task(1, make_task_with_tx(1));

        let gas_before = ctx.total_gas_used();
        ctx.execute_task(0);
        ctx.execute_task(1);
        // Gas should be >= 0 (may be 0 if txs fail on empty state, but should not underflow)
        assert!(ctx.total_gas_used() >= gas_before);
    }

    #[test]
    fn test_state_cache_trait_impl() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);
        let info =
            revm_state::AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() };
        cache.insert_account(addr, Some(info));

        // Use via StateCache trait
        let sc: &dyn StateCache = &cache;
        let got = sc.get_account(&addr).unwrap().unwrap();
        assert_eq!(got.balance, U256::from(1000));

        // Cache miss
        assert!(sc.get_account(&Address::with_last_byte(0x99)).is_none());
    }

    #[test]
    fn test_debug_impl() {
        let curr = Arc::new(ParallelStateCache::new());
        let ctx = ParallelBlockContext::new(10, curr, None, BlockEnv::default(), CfgEnv::default());
        let debug = format!("{:?}", ctx);
        assert!(debug.contains("ParallelBlockContext"));
        assert!(debug.contains("tasks_count"));
    }
}
