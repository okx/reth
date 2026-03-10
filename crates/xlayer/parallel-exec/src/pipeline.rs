//! Parallel execution pipeline orchestrator.
//!
//! Ties together: Simulator -> Framer -> Dashboard -> Dispatcher (rayon) -> Result collection
//!
//! Pipeline flow:
//! 1. Simulator pre-executes transactions to extract CrwSets
//! 2. Framer groups non-conflicting transactions into frames using ParaBloom
//! 3. Warmup phase computes EEI (Earliest Execution Index) for each task
//! 4. Dashboard-based cascade dispatch: tasks execute as soon as their dependency completes, with
//!    ignition propagating through the dependency graph
//! 5. Results are collected in original transaction order
//!
//! This is the true parallel execution approach (like fafo's ExePipe), NOT frame-serial.

use crate::{
    dashboard::{Dashboard, EARLY_EXE_WINDOW_SIZE, FIRST_FRAME},
    dispatcher_new::ParallelDispatcher,
    framer::Framer,
    parallel_state_cache::ParallelStateCache,
    simulator::{SimTxEnv, Simulator},
    task::ExeTask,
    tasks_manager::TasksManager,
};
use alloy_primitives::{Address, B256, U256};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm_state::AccountInfo;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Input transaction for the pipeline.
#[derive(Debug, Clone)]
pub struct PipelineTxInput {
    /// Sender address.
    pub sender: Address,
    /// Transaction environment for execution.
    pub tx_env: TxEnv,
    /// Original index in the block.
    pub original_index: usize,
}

/// Result of parallel block execution.
#[derive(Debug)]
pub struct PipelineBlockResult {
    /// Per-transaction results, ordered by original index.
    pub tx_results: Vec<PipelineTxResult>,
    /// Total gas used.
    pub total_gas_used: u64,
    /// Current block's state cache (can be reused as prev_state for next block).
    pub state_cache: Arc<ParallelStateCache>,
}

/// Result for a single transaction from the pipeline.
#[derive(Debug)]
pub struct PipelineTxResult {
    /// Original index in the block.
    pub original_index: usize,
    /// Execution result.
    pub result: revm::context::result::ExecutionResult,
    /// State changes.
    pub state: revm::state::EvmState,
    /// Gas used.
    pub gas_used: u64,
    /// Whether succeeded.
    pub success: bool,
}

// ---------------------------------------------------------------------------
// CachedDbRef: DatabaseRef adapter that reads from ParallelStateCache first,
// then falls back to a generic DatabaseRef. This ensures that tasks see
// state writes from completed predecessors (the Dashboard guarantees ordering).
// ---------------------------------------------------------------------------

/// A `DatabaseRef` that layers `ParallelStateCache` on top of a generic fallback.
///
/// This is the key to correctness in true parallel execution: when task B
/// depends on task A (via Dashboard's EEI), B will only execute after A
/// completes and applies its state to `ParallelStateCache`. B then reads
/// A's writes from the cache.
struct CachedDbRef<'a, DB> {
    cache: &'a ParallelStateCache,
    fallback: &'a DB,
}

impl<DB: core::fmt::Debug> core::fmt::Debug for CachedDbRef<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CachedDbRef").field("fallback", &self.fallback).finish()
    }
}

impl<DB> revm::DatabaseRef for CachedDbRef<'_, DB>
where
    DB: revm::DatabaseRef + core::fmt::Debug,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(info) = self.cache.get_account(&address) {
            return Ok(info);
        }
        self.fallback.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        if let Some(code) = self.cache.get_bytecode(&code_hash) {
            return Ok(code);
        }
        self.fallback.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.get_storage(&address, &index) {
            return Ok(value);
        }
        self.fallback.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(hash) = self.cache.get_block_hash(&number) {
            return Ok(hash);
        }
        self.fallback.block_hash_ref(number)
    }
}

/// Parallel execution pipeline.
///
/// Reusable across blocks. Owns the Simulator and Dispatcher thread pools.
///
/// Uses true Dashboard-based parallel execution with cascade ignition:
/// - Tasks are grouped into frames by the Framer (conflict detection via ParaBloom)
/// - EEI (Earliest Execution Index) is computed for each task
/// - Tasks with no dependencies execute immediately on the rayon pool
/// - Completed tasks "ignite" their dependents via the Dashboard's linked lists
/// - State changes are applied to ParallelStateCache after each task completes, making them visible
///   to subsequently-ignited dependent tasks
pub struct ParallelExecutionPipeline {
    /// Simulator for pre-execution (CrwSets extraction).
    simulator: Simulator,
    /// Dispatcher for parallel execution (owns rayon thread pools).
    dispatcher: ParallelDispatcher,
    /// Previous block's state cache (for cross-block optimization).
    prev_state: Option<Arc<ParallelStateCache>>,
}

impl ParallelExecutionPipeline {
    /// Create a new pipeline with default settings.
    /// - Simulator: 4 shards, 2 threads
    /// - Dispatcher: 12 warmup threads, 64 execution threads
    pub fn new() -> Self {
        Self {
            simulator: Simulator::new(),
            dispatcher: ParallelDispatcher::new(12, 64),
            prev_state: None,
        }
    }

    /// Create with custom thread counts.
    pub fn with_config(sim_shards: usize, warmup_threads: usize, exe_threads: usize) -> Self {
        Self {
            simulator: Simulator::with_shard_count(sim_shards),
            dispatcher: ParallelDispatcher::new(warmup_threads, exe_threads),
            prev_state: None,
        }
    }

    /// Execute a block of transactions in parallel.
    ///
    /// True parallel execution pipeline:
    /// 1. Convert inputs to SimTxEnvs
    /// 2. Simulate to extract CrwSets (parallel, using Simulator)
    /// 3. Frame non-conflicting transactions (sequential, using Framer+ParaBloom)
    /// 4. Flatten tasks, assign indices, compute task_out_start
    /// 5. Warmup: compute EEI for each task (backward collision scan)
    /// 6. Dashboard-based cascade execution on rayon pool
    /// 7. Collect results in original transaction order
    pub fn execute_block<DB>(
        &mut self,
        txs: Vec<PipelineTxInput>,
        db: &DB,
        block_env: &BlockEnv,
        cfg_env: &CfgEnv,
    ) -> PipelineBlockResult
    where
        DB: revm::DatabaseRef + core::fmt::Debug + Sync,
        DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
    {
        if txs.is_empty() {
            let cache = Arc::new(ParallelStateCache::new());
            return PipelineBlockResult {
                tx_results: vec![],
                total_gas_used: 0,
                state_cache: cache,
            };
        }

        let tx_count = txs.len();

        // 1. Convert to SimTxEnvs for simulation
        let sim_txs: Vec<SimTxEnv> =
            txs.iter().map(|t| SimTxEnv { sender: t.sender, tx_env: t.tx_env.clone() }).collect();

        // 2. Simulate to extract CrwSets
        let sim_results = self.simulator.simulate(&sim_txs, db, block_env);

        // 3. Frame using Framer
        let mut framer = Framer::new();
        for sim_result in sim_results {
            framer.add(sim_result);
        }
        let frames = framer.finish();

        // 4. Flatten tasks with indices and set task_out_start
        let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
        let tasks_manager = TasksManager::with_size(total_tasks);

        // Mapping: task_idx -> list of original tx indices (for result collection)
        let mut task_tx_mapping: Vec<Vec<usize>> = Vec::with_capacity(total_tasks);
        let mut task_idx = 0usize;

        for frame in frames {
            let frame_start = task_idx;
            for mut task in frame.tasks {
                // Populate tx_envs for actual execution
                for sim_result in &task.sim_results {
                    task.tx_envs.push(sim_txs[sim_result.original_index].tx_env.clone());
                }

                // Set task_out_start so EEI backward scan knows where to stop
                task.set_task_out_start(frame_start);

                let original_indices: Vec<usize> =
                    task.sim_results.iter().map(|sr| sr.original_index).collect();
                task_tx_mapping.push(original_indices);

                tasks_manager.set_task(task_idx, task);
                task_idx += 1;
            }
        }

        // 5. Warmup: compute EEI for each task via backward collision scan
        let dashboard = Dashboard::new(total_tasks);
        dashboard.set_valid_count(total_tasks as i32);

        let mut eeis = Vec::with_capacity(total_tasks);
        for idx in 0..total_tasks {
            let eei = compute_eei(idx, &tasks_manager);
            dashboard.set_eei(idx as i32, eei);
            dashboard.notify_warmed(idx as i32);
            eeis.push(eei);
        }

        // 6. True parallel execution via Dashboard cascade on rayon pool
        let curr_state = Arc::new(ParallelStateCache::new());
        let results: Vec<parking_lot::Mutex<Option<PipelineTxResult>>> =
            (0..tx_count).map(|_| parking_lot::Mutex::new(None)).collect();
        let total_gas = AtomicU64::new(0);

        // Execute on the dispatcher's rayon pool using rayon::scope for
        // safe reference sharing (no 'static bound needed).
        self.dispatcher.exe_pool_install(|| {
            rayon::scope(|scope| {
                // Spawn all tasks that have no dependencies (EEI == FIRST_FRAME)
                for idx in 0..total_tasks {
                    if eeis[idx] == FIRST_FRAME {
                        spawn_execute_task(
                            scope,
                            idx,
                            db,
                            &curr_state,
                            &dashboard,
                            &sim_txs,
                            block_env,
                            cfg_env,
                            &results,
                            &total_gas,
                            &task_tx_mapping,
                        );
                    }
                }
                // Tasks with dependencies are spawned via cascade ignition
                // when their dependency completes (in spawn_execute_task).
            });
        });

        // 7. Collect results in original transaction order
        let mut all_results: Vec<PipelineTxResult> = results
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                m.into_inner().unwrap_or_else(|| {
                    tracing::warn!(
                        target: "xlayer::parallel::pipeline",
                        index = i,
                        "Missing result for transaction, creating placeholder"
                    );
                    PipelineTxResult {
                        original_index: i,
                        result: revm::context::result::ExecutionResult::Halt {
                            reason: revm::context::result::HaltReason::NotActivated,
                            gas_used: 0,
                        },
                        state: Default::default(),
                        gas_used: 0,
                        success: false,
                    }
                })
            })
            .collect();

        all_results.sort_by_key(|r| r.original_index);

        // Update prev_state for next block
        self.prev_state = Some(curr_state.clone());

        PipelineBlockResult {
            tx_results: all_results,
            total_gas_used: total_gas.load(Ordering::Acquire),
            state_cache: curr_state,
        }
    }
}

/// Spawn a task for execution on the rayon scope, with cascade ignition.
///
/// After executing all transactions in the task:
/// 1. Applies state changes to `curr_state` (visible to future tasks)
/// 2. Marks the task as executed in the Dashboard
/// 3. Retrieves the ignited list (tasks waiting on this one)
/// 4. Recursively spawns ignited tasks for execution
fn spawn_execute_task<'s, DB>(
    scope: &rayon::Scope<'s>,
    task_idx: usize,
    db: &'s DB,
    curr_state: &'s ParallelStateCache,
    dashboard: &'s Dashboard,
    sim_txs: &'s [SimTxEnv],
    block_env: &'s BlockEnv,
    cfg_env: &'s CfgEnv,
    results: &'s [parking_lot::Mutex<Option<PipelineTxResult>>],
    total_gas: &'s AtomicU64,
    task_tx_mapping: &'s [Vec<usize>],
) where
    DB: revm::DatabaseRef + core::fmt::Debug + Sync,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    scope.spawn(move |s| {
        // Create a CachedDbRef that reads from curr_state first, then falls back to db.
        // This ensures we see writes from completed predecessors.
        let cached_db = CachedDbRef { cache: curr_state, fallback: db };

        // Execute all transactions in this task
        let original_indices = &task_tx_mapping[task_idx];
        for &orig_idx in original_indices {
            let tx_env = sim_txs[orig_idx].tx_env.clone();

            let result =
                crate::execute::execute_tx_with_ref(&cached_db, block_env, cfg_env, tx_env);

            // Apply state diff to shared cache BEFORE storing the result.
            // This makes the writes visible to tasks that depend on us.
            curr_state.apply_evm_state(&result.state);
            total_gas.fetch_add(result.gas_used, Ordering::Relaxed);

            *results[orig_idx].lock() = Some(PipelineTxResult {
                original_index: orig_idx,
                result: result.result,
                state: result.state,
                gas_used: result.gas_used,
                success: result.success,
            });
        }

        // Mark this task as executed in the Dashboard
        dashboard.set_executed(task_idx as i32);

        // Cascade ignition: spawn any tasks that were waiting on this one
        let ignited = dashboard.get_ignited_list(task_idx as i32);
        for dep_idx in ignited {
            spawn_execute_task(
                s,
                dep_idx as usize,
                db,
                curr_state,
                dashboard,
                sim_txs,
                block_env,
                cfg_env,
                results,
                total_gas,
                task_tx_mapping,
            );
        }
    });
}

/// Compute EEI (Earliest Execution Index) via backward collision scan.
///
/// Scans earlier tasks (within `EARLY_EXE_WINDOW_SIZE`) for read-write
/// collisions. Returns the index of the latest conflicting task, or
/// `FIRST_FRAME` if no dependency exists.
fn compute_eei(my_idx: usize, tasks_manager: &TasksManager) -> i32 {
    let task_guard = tasks_manager.task_for_read(my_idx);
    let task = match task_guard.as_ref() {
        Some(t) => t,
        None => return FIRST_FRAME,
    };

    let task_out_start = task.get_task_out_start();
    if task_out_start == 0 {
        return FIRST_FRAME;
    }

    // Backward scan window
    let stop = if my_idx > EARLY_EXE_WINDOW_SIZE { my_idx - EARLY_EXE_WINDOW_SIZE } else { 0 };

    let mut eei = if stop == 0 { FIRST_FRAME } else { (stop - 1) as i32 };

    for earlier_idx in (stop..task_out_start).rev() {
        let other_guard = tasks_manager.task_for_read(earlier_idx);
        if let Some(other) = other_guard.as_ref() {
            if ExeTask::has_collision(task, other) {
                eei = earlier_idx as i32;
                break;
            }
        }
    }

    drop(task_guard);
    eei
}

impl Default for ParallelExecutionPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ParallelExecutionPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelExecutionPipeline")
            .field("has_prev_state", &self.prev_state.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxKind, U256};

    fn make_pipeline_tx(
        sender: Address,
        recipient: Address,
        idx: usize,
        nonce: u64,
    ) -> PipelineTxInput {
        PipelineTxInput {
            sender,
            tx_env: TxEnv {
                caller: sender,
                gas_limit: 21000,
                gas_price: 0,
                kind: TxKind::Call(recipient),
                value: U256::ZERO,
                nonce,
                ..Default::default()
            },
            original_index: idx,
        }
    }

    #[test]
    fn test_pipeline_creation() {
        let pipeline = ParallelExecutionPipeline::new();
        assert!(pipeline.prev_state.is_none());
    }

    #[test]
    fn test_pipeline_empty_block() {
        let mut pipeline = ParallelExecutionPipeline::new();
        let db = revm::database::EmptyDB::default();
        let result = pipeline.execute_block(vec![], &db, &BlockEnv::default(), &CfgEnv::default());
        assert_eq!(result.tx_results.len(), 0);
        assert_eq!(result.total_gas_used, 0);
    }

    #[test]
    fn test_pipeline_single_tx() {
        let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
        let db = revm::database::EmptyDB::default();
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let sender = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let txs = vec![make_pipeline_tx(sender, recipient, 0, 0)];

        let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);
        assert_eq!(result.tx_results.len(), 1);
        assert_eq!(result.tx_results[0].original_index, 0);
    }

    #[test]
    fn test_pipeline_multiple_txs_preserve_order() {
        let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
        let db = revm::database::EmptyDB::default();
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let txs: Vec<PipelineTxInput> = (0..5u8)
            .map(|i| {
                make_pipeline_tx(
                    Address::with_last_byte(i),
                    Address::with_last_byte(i + 100),
                    i as usize,
                    0,
                )
            })
            .collect();

        let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);
        assert_eq!(result.tx_results.len(), 5);
        for (i, tx_result) in result.tx_results.iter().enumerate() {
            assert_eq!(tx_result.original_index, i);
        }
    }

    #[test]
    fn test_pipeline_prev_state_updated() {
        let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
        let db = revm::database::EmptyDB::default();
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        // First block
        let txs =
            vec![make_pipeline_tx(Address::with_last_byte(1), Address::with_last_byte(2), 0, 0)];
        let _ = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);
        assert!(pipeline.prev_state.is_some());

        // Second block should have prev_state
        let txs2 =
            vec![make_pipeline_tx(Address::with_last_byte(3), Address::with_last_byte(4), 0, 0)];
        let _ = pipeline.execute_block(txs2, &db, &BlockEnv::default(), &cfg);
        assert!(pipeline.prev_state.is_some());
    }

    #[test]
    fn test_pipeline_uses_dashboard_cascade() {
        // Test that the pipeline correctly handles dependent transactions.
        // Tx 0 and Tx 1 both touch the same address (same sender), so they
        // should end up in different frames with a dependency between them.
        let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
        let db = revm::database::EmptyDB::default();
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let sender = Address::with_last_byte(0xAA);
        let recipient1 = Address::with_last_byte(0xBB);
        let recipient2 = Address::with_last_byte(0xCC);

        let txs = vec![
            make_pipeline_tx(sender, recipient1, 0, 0),
            make_pipeline_tx(sender, recipient2, 1, 1),
        ];

        let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);
        assert_eq!(result.tx_results.len(), 2);
        assert_eq!(result.tx_results[0].original_index, 0);
        assert_eq!(result.tx_results[1].original_index, 1);
    }

    #[test]
    fn test_pipeline_independent_txs_parallel() {
        // Many independent transactions (different senders, different recipients)
        // should all execute in parallel (all in frame 0, all EEI = FIRST_FRAME).
        let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 8);
        let db = revm::database::EmptyDB::default();
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let txs: Vec<PipelineTxInput> = (0..20u8)
            .map(|i| {
                make_pipeline_tx(
                    Address::with_last_byte(i),
                    Address::with_last_byte(i + 200),
                    i as usize,
                    0,
                )
            })
            .collect();

        let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);
        assert_eq!(result.tx_results.len(), 20);
        for (i, tx_result) in result.tx_results.iter().enumerate() {
            assert_eq!(tx_result.original_index, i);
        }
    }

    #[test]
    fn test_cached_db_ref_reads_from_cache() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);
        let info = AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() };
        cache.insert_account(addr, Some(info));

        let db = revm::database::EmptyDB::default();
        let cached = CachedDbRef { cache: &cache, fallback: &db };

        use revm::DatabaseRef;
        let result = cached.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(1000));
        assert_eq!(result.nonce, 5);
    }

    #[test]
    fn test_cached_db_ref_falls_through() {
        let cache = ParallelStateCache::new();
        let db = revm::database::EmptyDB::default();
        let cached = CachedDbRef { cache: &cache, fallback: &db };

        use revm::DatabaseRef;
        // EmptyDB returns None for unknown accounts
        let result = cached.basic_ref(Address::with_last_byte(0xFF)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_compute_eei_no_task() {
        let tm = TasksManager::with_size(10);
        assert_eq!(compute_eei(0, &tm), FIRST_FRAME);
    }

    #[test]
    fn test_compute_eei_first_frame() {
        let tm = TasksManager::with_size(10);
        let task = ExeTask::new(crate::task::SimResult {
            crw_sets: crate::crw_sets::CrwSets::default(),
            original_index: 0,
            success: true,
        });
        tm.set_task(0, task);
        // task_out_start = 0 (first frame) -> FIRST_FRAME
        assert_eq!(compute_eei(0, &tm), FIRST_FRAME);
    }

    #[test]
    fn test_compute_eei_with_collision() {
        let tm = TasksManager::with_size(10);

        // Task 0: writes [1u8; 10]
        let task0 = ExeTask::new(crate::task::SimResult {
            crw_sets: crate::crw_sets::CrwSets {
                account_reads: vec![],
                account_writes: vec![[1u8; 10]],
                storage_reads: vec![],
                storage_writes: vec![],
            },
            original_index: 0,
            success: true,
        });
        tm.set_task(0, task0);

        // Task 1: also writes [1u8; 10], task_out_start = 1
        let task1 = ExeTask::new(crate::task::SimResult {
            crw_sets: crate::crw_sets::CrwSets {
                account_reads: vec![],
                account_writes: vec![[1u8; 10]],
                storage_reads: vec![],
                storage_writes: vec![],
            },
            original_index: 1,
            success: true,
        });
        task1.set_task_out_start(1);
        tm.set_task(1, task1);

        // Task 1 should have EEI = 0 (collision with task 0)
        assert_eq!(compute_eei(1, &tm), 0);
    }
}
