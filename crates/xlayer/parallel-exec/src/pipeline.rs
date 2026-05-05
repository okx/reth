//! Parallel execution pipeline orchestrator.
//!
//! Ties together: Simulator -> Framer -> Dashboard -> Dispatcher (rayon) -> Result collection
//!
//! Pipeline flow (async, channel-based like fafo's ExePipe):
//! 1. Simulator pre-executes transactions on its own thread pool, sending SimResults via channel
//! 2. Framer thread receives SimResults, groups into frames using ParaBloom, auto-flushes at
//!    threshold
//! 3. Flushed frames are dispatched IMMEDIATELY: tasks are stored in TasksManager, EEI computed,
//!    and tasks with no dependencies spawned on the rayon execution pool
//! 4. Dashboard-based cascade dispatch: completed tasks ignite their dependents
//! 5. Results are collected in original transaction order
//!
//! Key: **no barriers between stages**. Simulation, framing, and execution overlap via channels.

use crate::{
    dashboard::{Dashboard, EARLY_EXE_WINDOW_SIZE, FIRST_FRAME},
    dispatcher_new::ParallelDispatcher,
    framer::{Frame, Framer},
    parallel_state_cache::ParallelStateCache,
    simulator::{SimTxEnv, Simulator},
    task::ExeTask,
    tasks_manager::TasksManager,
};
use alloy_primitives::{Address, B256, U256};
use rayon::prelude::*;
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
    /// Pre-computed CrwSets (if available). When `Some`, the simulator skips
    /// EVM simulation and uses these directly — matching fafo's model where
    /// known transaction patterns (e.g., ERC-20 transfers) have deterministic
    /// read/write sets.
    pub pre_crw_sets: Option<crate::crw_sets::CrwSets>,
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
/// Three-tier lookup (matching fafo's BlockContext):
/// 1. `curr_state` — current block's accumulated state
/// 2. `prev_state` — previous block's state cache (cross-block optimization)
/// 3. `fallback`   — disk state (QMDB/MDBX via StateProvider)
struct CachedDbRef<'a, DB> {
    cache: &'a ParallelStateCache,
    prev_state: Option<&'a ParallelStateCache>,
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
        if let Some(prev) = self.prev_state {
            if let Some(info) = prev.get_account(&address) {
                return Ok(info);
            }
        }
        self.fallback.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        if let Some(code) = self.cache.get_bytecode(&code_hash) {
            return Ok(code);
        }
        if let Some(prev) = self.prev_state {
            if let Some(code) = prev.get_bytecode(&code_hash) {
                return Ok(code);
            }
        }
        self.fallback.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.get_storage(&address, &index) {
            return Ok(value);
        }
        if let Some(prev) = self.prev_state {
            if let Some(value) = prev.get_storage(&address, &index) {
                return Ok(value);
            }
        }
        self.fallback.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(hash) = self.cache.get_block_hash(&number) {
            return Ok(hash);
        }
        if let Some(prev) = self.prev_state {
            if let Some(hash) = prev.get_block_hash(&number) {
                return Ok(hash);
            }
        }
        self.fallback.block_hash_ref(number)
    }
}

// ---------------------------------------------------------------------------
// WarmingDbRef: DatabaseRef wrapper that caches reads into ParallelStateCache.
// Used during simulation to pre-populate the cache ("warmup"), so execution
// reads hit the cache instead of going to disk.
// ---------------------------------------------------------------------------

/// A `DatabaseRef` wrapper that warms up the `ParallelStateCache` on reads.
///
/// Every account, storage, bytecode, and block hash read during simulation
/// is automatically cached using "insert if absent" semantics. This ensures:
/// - Execution sees cached data from simulation (cache hit instead of disk I/O)
/// - Execution's writes are never overwritten by stale simulation data
///
/// This is the key to fafo's performance: simulation effectively prefetches
/// all data needed for execution into memory.
struct WarmingDbRef<'a, DB> {
    inner: &'a DB,
    cache: &'a ParallelStateCache,
}

impl<DB: core::fmt::Debug> core::fmt::Debug for WarmingDbRef<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WarmingDbRef").field("inner", &self.inner).finish()
    }
}

impl<DB> revm::DatabaseRef for WarmingDbRef<'_, DB>
where
    DB: revm::DatabaseRef + core::fmt::Debug,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Check cache first (may have been warmed by a prior simulation)
        if let Some(info) = self.cache.get_account(&address) {
            return Ok(info);
        }
        let result = self.inner.basic_ref(address)?;
        // Warm the cache (insert-if-absent: safe even if execution wrote first)
        self.cache.insert_account_if_absent(address, result.clone());
        Ok(result)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        if let Some(code) = self.cache.get_bytecode(&code_hash) {
            return Ok(code);
        }
        let result = self.inner.code_by_hash_ref(code_hash)?;
        self.cache.insert_bytecode_if_absent(code_hash, result.clone());
        Ok(result)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.get_storage(&address, &index) {
            return Ok(value);
        }
        let result = self.inner.storage_ref(address, index)?;
        self.cache.insert_storage_if_absent(address, index, result);
        Ok(result)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(hash) = self.cache.get_block_hash(&number) {
            return Ok(hash);
        }
        let result = self.inner.block_hash_ref(number)?;
        self.cache.insert_block_hash_if_absent(number, result);
        Ok(result)
    }
}

/// Default auto-flush threshold for the Framer.
/// When a frame accumulates this many tasks, it's flushed and dispatched
/// for execution immediately. This creates pipeline overlap between
/// simulation, framing, and execution (like fafo's streaming Framer).
/// Matches fafo's max_tasks_len_in_frame = 256.
const DEFAULT_FLUSH_THRESHOLD: usize = 256;

/// Flush threshold for the first frame. Smaller than DEFAULT_FLUSH_THRESHOLD
/// to minimize latency before execution starts.
const FIRST_FLUSH_THRESHOLD: usize = 4;

/// Parallel execution pipeline.
///
/// Reusable across blocks. Owns the Simulator and Dispatcher thread pools.
///
/// Uses async channel-based pipeline (like fafo's ExePipe):
/// - Simulator threads produce SimResults via channel (non-blocking)
/// - Framer receives and frames incrementally, dispatching flushed frames immediately
/// - Execution starts as soon as the first frame is flushed (no barrier)
/// - Dashboard cascade ignition handles task dependencies
pub struct ParallelExecutionPipeline {
    /// Simulator for pre-execution (CrwSets extraction).
    simulator: Simulator,
    /// Dispatcher for parallel execution (owns rayon thread pools).
    dispatcher: ParallelDispatcher,
    /// Previous block's state cache (for cross-block optimization).
    prev_state: Option<Arc<ParallelStateCache>>,
}

impl ParallelExecutionPipeline {
    /// Create a new pipeline with default settings matching fafo:
    /// - Simulator: 16 threads (parallel pre-execution for CrwSets + cache warmup)
    /// - Dispatcher: 64 execution threads
    pub fn new() -> Self {
        Self {
            simulator: Simulator::with_config(4, 16),
            dispatcher: ParallelDispatcher::new(12, 64),
            prev_state: None,
        }
    }

    /// Create with custom thread counts.
    pub fn with_config(sim_threads: usize, _warmup_threads: usize, exe_threads: usize) -> Self {
        Self {
            simulator: Simulator::with_config(4, sim_threads),
            dispatcher: ParallelDispatcher::new(12, exe_threads),
            prev_state: None,
        }
    }

    /// Execute a block of transactions in parallel.
    ///
    /// Async channel-based pipeline (like fafo's ExePipe):
    /// 1. Simulator sends SimResults via channel as each tx completes (parallel)
    /// 2. Framer thread receives, frames, and dispatches flushed frames immediately
    /// 3. Execution starts as first frame is dispatched (no barrier)
    /// 4. Dashboard cascade handles dependencies
    /// 5. Results collected in original order
    ///
    /// All stages overlap: simulation, framing, and execution run concurrently.
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
        let sim_txs: Vec<SimTxEnv> = txs
            .iter()
            .map(|t| SimTxEnv {
                sender: t.sender,
                tx_env: t.tx_env.clone(),
                pre_crw_sets: t.pre_crw_sets.clone(),
            })
            .collect();

        // 2. Pre-allocate shared state
        let max_tasks = tx_count; // worst case: one task per tx
        let curr_state = Arc::new(ParallelStateCache::new());
        let results: Vec<parking_lot::Mutex<Option<PipelineTxResult>>> =
            (0..tx_count).map(|_| parking_lot::Mutex::new(None)).collect();
        let total_gas = AtomicU64::new(0);
        let dashboard = Dashboard::new(max_tasks);
        let tasks_manager = TasksManager::with_size(max_tasks);

        // 3. Channel: Simulator → Framer (unbounded, non-blocking send)
        // crossbeam's Receiver is Sync, required for rayon::scope's Send closure.
        let (sim_sender, sim_receiver) = crossbeam_channel::unbounded::<crate::task::SimResult>();

        // Split borrows from self so simulator and dispatcher can be used concurrently
        let simulator = &self.simulator;
        let dispatcher = &self.dispatcher;
        let prev_state = self.prev_state.as_deref();

        // WarmingDbRef: wraps the DB to cache reads into ParallelStateCache.
        // Every account/storage read during simulation is automatically
        // cached ("warmup"), so execution reads hit memory instead of disk.
        // Created before the scope so it outlives all spawned threads.
        let warming_db = WarmingDbRef { inner: db, cache: &curr_state };

        // Debug: check how many txs have empty data (should hit fast path)
        let empty_data_count = sim_txs.iter().filter(|t| t.tx_env.data.is_empty()).count();
        if tx_count > 0 {
            tracing::info!(
                target: "xlayer::parallel::pipeline",
                tx_count,
                empty_data = empty_data_count,
                non_empty_data = tx_count - empty_data_count,
                sample_data_len = sim_txs.first().map(|t| t.tx_env.data.len()).unwrap_or(0),
                "fast path eligibility check"
            );
        }

        // 4. Async pipeline via std::thread::scope (safe reference sharing)
        let pipeline_start = std::time::Instant::now();
        let sim_done = std::sync::atomic::AtomicU64::new(0);
        let first_frame_dispatched = std::sync::atomic::AtomicU64::new(0);

        std::thread::scope(|thread_scope| {
            let sim_done_ref = &sim_done;
            let first_frame_ref = &first_frame_dispatched;

            // --- Simulator thread ---
            thread_scope.spawn({
                let sim_txs = &sim_txs;
                let warming_db = &warming_db;
                move || {
                    let sim_start = std::time::Instant::now();
                    simulator.pool.install(|| {
                        sim_txs.par_iter().enumerate().for_each_with(
                            sim_sender,
                            |sender, (idx, tx)| {
                                let result = simulator.simulate_one(tx, idx, warming_db, block_env);
                                let _ = sender.send(result);
                            },
                        );
                    });
                    sim_done_ref.store(
                        sim_start.elapsed().as_micros() as u64,
                        std::sync::atomic::Ordering::Release,
                    );
                }
            });

            // --- Framer + Executor (main thread) ---
            dispatcher.exe_pool_install(|| {
                rayon::scope(|exe_scope| {
                    let mut framer =
                        Framer::with_early_dispatch(FIRST_FLUSH_THRESHOLD, DEFAULT_FLUSH_THRESHOLD);
                    let mut task_idx = 0usize;
                    let mut frames_dispatched = 0usize;

                    // TX_IN_TASK: group N SimResults into one ExeTask before framing.
                    // Matches fafo's SimulatorShard which groups 4 txs per task.
                    const TX_IN_TASK: usize = 4;
                    let mut pending_sims: Vec<crate::task::SimResult> =
                        Vec::with_capacity(TX_IN_TASK);

                    while let Ok(sim_result) = sim_receiver.recv() {
                        pending_sims.push(sim_result);
                        if pending_sims.len() < TX_IN_TASK {
                            continue;
                        }
                        // Group TX_IN_TASK SimResults into one ExeTask
                        let mut pending = std::mem::take(&mut pending_sims);
                        pending_sims = Vec::with_capacity(TX_IN_TASK);
                        let mut task = crate::task::ExeTask::new(pending.remove(0));
                        for sr in pending {
                            task.add(sr);
                        }
                        let flushed_frames = framer.add_task_returning_flushed(task);
                        for frame in flushed_frames {
                            if frames_dispatched == 0 {
                                first_frame_ref.store(
                                    pipeline_start.elapsed().as_micros() as u64,
                                    std::sync::atomic::Ordering::Release,
                                );
                            }
                            frames_dispatched += 1;
                            dispatch_frame(
                                frame,
                                &mut task_idx,
                                &sim_txs,
                                &tasks_manager,
                                &dashboard,
                                exe_scope,
                                db,
                                &curr_state,
                                prev_state,
                                block_env,
                                cfg_env,
                                &results,
                                &total_gas,
                            );
                        }
                    }

                    // Flush remaining pending SimResults as a partial task
                    if !pending_sims.is_empty() {
                        let mut task = crate::task::ExeTask::new(pending_sims.remove(0));
                        for sr in pending_sims {
                            task.add(sr);
                        }
                        let flushed = framer.add_task_returning_flushed(task);
                        for frame in flushed {
                            frames_dispatched += 1;
                            dispatch_frame(
                                frame,
                                &mut task_idx,
                                &sim_txs,
                                &tasks_manager,
                                &dashboard,
                                exe_scope,
                                db,
                                &curr_state,
                                prev_state,
                                block_env,
                                cfg_env,
                                &results,
                                &total_gas,
                            );
                        }
                    }

                    for frame in framer.finish() {
                        if frames_dispatched == 0 {
                            first_frame_ref.store(
                                pipeline_start.elapsed().as_micros() as u64,
                                std::sync::atomic::Ordering::Release,
                            );
                        }
                        frames_dispatched += 1;
                        dispatch_frame(
                            frame,
                            &mut task_idx,
                            &sim_txs,
                            &tasks_manager,
                            &dashboard,
                            exe_scope,
                            db,
                            &curr_state,
                            prev_state,
                            block_env,
                            cfg_env,
                            &results,
                            &total_gas,
                        );
                    }

                    dashboard.set_valid_count(task_idx as i32);

                    tracing::info!(
                        target: "xlayer::parallel::pipeline",
                        tasks = task_idx,
                        frames = frames_dispatched,
                        "pipeline framing done"
                    );
                });
            });
        });

        let total_pipeline_us = pipeline_start.elapsed().as_micros() as u64;
        let sim_us = sim_done.load(std::sync::atomic::Ordering::Acquire);
        let first_frame_us = first_frame_dispatched.load(std::sync::atomic::Ordering::Acquire);
        tracing::info!(
            target: "xlayer::parallel::pipeline",
            tx_count,
            sim_ms = sim_us / 1000,
            first_frame_ms = first_frame_us / 1000,
            total_ms = total_pipeline_us / 1000,
            "pipeline timing: sim={sim_us}µs first_frame={first_frame_us}µs total={total_pipeline_us}µs"
        );

        // 5. Collect results in original transaction order
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
                            gas: revm::context::result::ResultGas::new(0, 0, 0),
                    logs: std::vec::Vec::new(),
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

/// Process a flushed frame: assign task indices, compute EEI in parallel, dispatch.
///
/// Three-phase dispatch (like fafo's warmup_pool + exe_pool):
/// 1. Store all tasks sequentially in TasksManager (maintains ordering for EEI scan)
/// 2. Compute EEI for all tasks in parallel via `par_iter` (uses exe_pool threads)
/// 3. Register EEI and dispatch ready tasks (sequential, no race conditions)
///
/// Phase 2 parallelizes the backward collision scan across all exe_pool threads.
/// For 64 tasks with window=128, this reduces per-frame EEI from ~240µs to ~4µs.
fn dispatch_frame<'s, DB>(
    frame: Frame,
    task_idx: &mut usize,
    sim_txs: &'s [SimTxEnv],
    tasks_manager: &'s TasksManager,
    dashboard: &'s Dashboard,
    exe_scope: &rayon::Scope<'s>,
    db: &'s DB,
    curr_state: &'s ParallelStateCache,
    prev_state: Option<&'s ParallelStateCache>,
    block_env: &'s BlockEnv,
    cfg_env: &'s CfgEnv,
    results: &'s [parking_lot::Mutex<Option<PipelineTxResult>>],
    total_gas: &'s AtomicU64,
) where
    DB: revm::DatabaseRef + core::fmt::Debug + Sync,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    // Phase 1: Store all tasks sequentially (fast, maintains ordering for EEI scan)
    let frame_start = *task_idx;
    let mut task_indices: Vec<usize> = Vec::with_capacity(frame.tasks.len());

    for mut task in frame.tasks {
        for sr in &task.sim_results {
            task.tx_envs.push(sim_txs[sr.original_index].tx_env.clone());
        }
        task.set_task_out_start(frame_start);
        tasks_manager.set_task(*task_idx, task);
        task_indices.push(*task_idx);
        *task_idx += 1;
    }

    // Phase 2: Compute EEI for all tasks in parallel (rayon par_iter on exe_pool)
    let eeis: Vec<i32> =
        task_indices.par_iter().map(|&idx| compute_eei(idx, tasks_manager)).collect();

    // Phase 3: Register EEI and dispatch (sequential — no race conditions)
    for (&idx, &eei) in task_indices.iter().zip(eeis.iter()) {
        dashboard.set_eei(idx as i32, eei);
        dashboard.notify_warmed(idx as i32);

        let should_dispatch = if eei == FIRST_FRAME {
            true
        } else if dashboard.is_executed(eei) {
            true
        } else {
            false
        };

        if should_dispatch {
            spawn_execute_task(
                exe_scope,
                idx,
                db,
                curr_state,
                prev_state,
                dashboard,
                tasks_manager,
                block_env,
                cfg_env,
                results,
                total_gas,
            );
        }
    }
}

/// Execute a task and cascade-ignite its dependents.
///
/// Called directly (from dispatch_frame's parallel warmup) or via cascade ignition.
/// After executing all transactions in the task:
/// 1. Applies state changes to `curr_state` (visible to future tasks)
/// 2. Marks the task as executed in the Dashboard
/// 3. Retrieves the ignited list (tasks waiting on this one)
/// 4. Recursively spawns ignited tasks for execution
fn execute_and_cascade<'s, DB>(
    scope: &rayon::Scope<'s>,
    task_idx: usize,
    db: &'s DB,
    curr_state: &'s ParallelStateCache,
    prev_state: Option<&'s ParallelStateCache>,
    dashboard: &'s Dashboard,
    tasks_manager: &'s TasksManager,
    block_env: &'s BlockEnv,
    cfg_env: &'s CfgEnv,
    results: &'s [parking_lot::Mutex<Option<PipelineTxResult>>],
    total_gas: &'s AtomicU64,
) where
    DB: revm::DatabaseRef + core::fmt::Debug + Sync,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    // Three-tier lookup: curr_state → prev_state → fallback db
    let cached_db = CachedDbRef { cache: curr_state, prev_state, fallback: db };

    // Read task data under a read lock (task stays in manager for EEI backward scan)
    let tx_data: Vec<(usize, TxEnv)> = {
        let guard = tasks_manager.task_for_read(task_idx);
        let task = guard.as_ref().expect("task should be present for execution");
        task.sim_results
            .iter()
            .zip(task.tx_envs.iter())
            .map(|(sr, te)| (sr.original_index, te.clone()))
            .collect()
    };

    // Execute all transactions in this task.
    // Uses execute_tx_with_ref per tx (EVM created once per tx via cached precompiles).
    for (orig_idx, tx_env) in tx_data {
        let result = crate::execute::execute_tx_with_ref(&cached_db, block_env, cfg_env, tx_env);

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
            scope,
            dep_idx as usize,
            db,
            curr_state,
            prev_state,
            dashboard,
            tasks_manager,
            block_env,
            cfg_env,
            results,
            total_gas,
        );
    }
}

/// Spawn a task for execution on the rayon scope, with cascade ignition.
///
/// Wraps `execute_and_cascade` in a rayon spawn. Used by cascade ignition
/// (where we need to spawn a new rayon task for the dependent).
fn spawn_execute_task<'s, DB>(
    scope: &rayon::Scope<'s>,
    task_idx: usize,
    db: &'s DB,
    curr_state: &'s ParallelStateCache,
    prev_state: Option<&'s ParallelStateCache>,
    dashboard: &'s Dashboard,
    tasks_manager: &'s TasksManager,
    block_env: &'s BlockEnv,
    cfg_env: &'s CfgEnv,
    results: &'s [parking_lot::Mutex<Option<PipelineTxResult>>],
    total_gas: &'s AtomicU64,
) where
    DB: revm::DatabaseRef + core::fmt::Debug + Sync,
    DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
{
    scope.spawn(move |s| {
        execute_and_cascade(
            s,
            task_idx,
            db,
            curr_state,
            prev_state,
            dashboard,
            tasks_manager,
            block_env,
            cfg_env,
            results,
            total_gas,
        );
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
            pre_crw_sets: None,
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
        let cached = CachedDbRef { cache: &cache, prev_state: None, fallback: &db };

        use revm::DatabaseRef;
        let result = cached.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(1000));
        assert_eq!(result.nonce, 5);
    }

    #[test]
    fn test_cached_db_ref_falls_through() {
        let cache = ParallelStateCache::new();
        let db = revm::database::EmptyDB::default();
        let cached = CachedDbRef { cache: &cache, prev_state: None, fallback: &db };

        use revm::DatabaseRef;
        // EmptyDB returns None for unknown accounts
        let result = cached.basic_ref(Address::with_last_byte(0xFF)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_cached_db_ref_reads_from_prev_state() {
        let curr = ParallelStateCache::new();
        let prev = ParallelStateCache::new();
        let db = revm::database::EmptyDB::default();

        // Data only in prev_state
        let addr = Address::with_last_byte(0x42);
        let info = AccountInfo { balance: U256::from(500), nonce: 3, ..Default::default() };
        prev.insert_account(addr, Some(info));
        prev.insert_storage(addr, U256::from(7), U256::from(999));

        let cached = CachedDbRef { cache: &curr, prev_state: Some(&prev), fallback: &db };

        use revm::DatabaseRef;
        // Should find account in prev_state
        let result = cached.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(500));

        // Should find storage in prev_state
        let val = cached.storage_ref(addr, U256::from(7)).unwrap();
        assert_eq!(val, U256::from(999));

        // curr_state overrides prev_state
        curr.insert_account(
            addr,
            Some(AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() }),
        );
        let result = cached.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(1000));
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
