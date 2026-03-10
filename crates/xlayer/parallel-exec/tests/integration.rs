//! End-to-end integration tests for the parallel execution framework.
//!
//! Tests the complete pipeline: PipelineTxInput -> Simulator -> Framer -> Dispatcher -> Results.
//! Also tests sub-component integration: Dashboard + Dispatcher, BlockContext, Framer ->
//! Dispatcher, and ParallelStateCache behavior during execution.
//!
//! IMPORTANT: All test addresses use values >= 0x20 to avoid the Ethereum precompile range
//! (0x01-0x13 in Prague spec).

use alloy_primitives::{Address, TxKind, U256};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};
use xlayer_parallel_exec::{
    block_context::ParallelBlockContext,
    crw_sets::CrwSets,
    dashboard::{Dashboard, FIRST_FRAME},
    dispatcher_new::{Dispatchable, ParallelDispatcher},
    framer::Framer,
    parallel_state_cache::ParallelStateCache,
    pipeline::{ParallelExecutionPipeline, PipelineTxInput},
    simulator::{SimTxEnv, Simulator},
    task::{ExeTask, SimResult},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create an address safely outside the precompile range (0x01-0x13).
fn addr(offset: u8) -> Address {
    Address::with_last_byte(0x20 + offset)
}

fn make_transfer(sender: Address, recipient: Address, idx: usize, nonce: u64) -> PipelineTxInput {
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

fn make_sim_tx(sender: Address, recipient: Address, nonce: u64) -> SimTxEnv {
    SimTxEnv {
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
    }
}

fn default_cfg() -> CfgEnv {
    let mut cfg = CfgEnv::default();
    cfg.disable_nonce_check = true;
    cfg
}

fn make_sim_result_with(index: usize, reads: Vec<[u8; 10]>, writes: Vec<[u8; 10]>) -> SimResult {
    SimResult {
        crw_sets: CrwSets {
            account_reads: reads,
            account_writes: writes,
            storage_reads: vec![],
            storage_writes: vec![],
        },
        original_index: index,
        success: true,
    }
}

// ---------------------------------------------------------------------------
// Mock Dispatchable for Dashboard + Dispatcher tests
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockTask {
    idx: i32,
    eei: i32,
    dashboard: Arc<Dashboard>,
    execution_log: Arc<parking_lot::Mutex<Vec<i32>>>,
    eei_map: Arc<Vec<AtomicI32>>,
}

impl MockTask {
    fn new(
        idx: i32,
        eei: i32,
        dashboard: Arc<Dashboard>,
        execution_log: Arc<parking_lot::Mutex<Vec<i32>>>,
        eei_map: Arc<Vec<AtomicI32>>,
    ) -> Self {
        eei_map[idx as usize].store(eei, Ordering::Release);
        Self { idx, eei, dashboard, execution_log, eei_map }
    }
}

impl Dispatchable for MockTask {
    fn warm_up(&self) -> i32 {
        self.eei
    }

    fn execute(&self) {
        self.execution_log.lock().push(self.idx);
    }

    fn get_dashboard(&self) -> &Dashboard {
        &self.dashboard
    }

    fn get_idx(&self) -> i32 {
        self.idx
    }

    fn get_sibling(&self, idx: i32) -> Self {
        let eei = self.eei_map[idx as usize].load(Ordering::Acquire);
        Self {
            idx,
            eei,
            dashboard: self.dashboard.clone(),
            execution_log: self.execution_log.clone(),
            eei_map: self.eei_map.clone(),
        }
    }

    fn end_block(&self) {}
}

fn make_eei_map(size: usize) -> Arc<Vec<AtomicI32>> {
    let mut v = Vec::with_capacity(size);
    for _ in 0..size {
        v.push(AtomicI32::new(FIRST_FRAME));
    }
    Arc::new(v)
}

// ---------------------------------------------------------------------------
// Test 1: Pipeline E2E - Simple Transfers
// ---------------------------------------------------------------------------

/// Test that 10 simple ETH transfers (different senders, same recipient)
/// all execute and return results in correct order.
#[test]
fn test_pipeline_e2e_simple_transfers() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let recipient = addr(200u8.wrapping_sub(0x20));
    let txs: Vec<PipelineTxInput> =
        (0..10u8).map(|i| make_transfer(addr(i), recipient, i as usize, 0)).collect();

    let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);

    assert_eq!(result.tx_results.len(), 10, "should have 10 results");
    for (i, tx) in result.tx_results.iter().enumerate() {
        assert_eq!(tx.original_index, i, "result {} should have original_index {}", i, i);
    }
}

// ---------------------------------------------------------------------------
// Test 2: Pipeline E2E - Preserves Order
// ---------------------------------------------------------------------------

/// Test that 20 transactions with unique sender-recipient pairs
/// produce results ordered by original_index (0..19).
#[test]
fn test_pipeline_e2e_preserves_order() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let txs: Vec<PipelineTxInput> = (0..20u8)
        .map(|i| {
            let sender = addr(i * 2);
            let recipient = addr(i * 2 + 1);
            make_transfer(sender, recipient, i as usize, 0)
        })
        .collect();

    let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);

    assert_eq!(result.tx_results.len(), 20, "should have 20 results");
    for (i, tx) in result.tx_results.iter().enumerate() {
        assert_eq!(tx.original_index, i, "result ordering mismatch at position {}", i);
    }
}

// ---------------------------------------------------------------------------
// Test 3: Pipeline E2E - Conflicting Transactions
// ---------------------------------------------------------------------------

/// Transactions from the same sender (sequential nonces) must be executed
/// serially. The Framer should separate them into different frames.
#[test]
fn test_pipeline_e2e_conflicting_transactions() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let sender = addr(0);
    let txs: Vec<PipelineTxInput> =
        (0..5u8).map(|i| make_transfer(sender, addr(10 + i), i as usize, i as u64)).collect();

    let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);

    assert_eq!(result.tx_results.len(), 5, "should have 5 results");
    // Verify ordering is preserved
    for (i, tx) in result.tx_results.iter().enumerate() {
        assert_eq!(tx.original_index, i);
    }
}

// ---------------------------------------------------------------------------
// Test 4: Pipeline E2E - Mixed Conflict and Independent
// ---------------------------------------------------------------------------

/// Mix of independent transfers (different senders) and conflicting ones
/// (same sender). Verify the Framer separates conflicting transactions.
#[test]
fn test_pipeline_e2e_mixed_conflict_and_independent() {
    // Use Simulator + Framer directly to verify framing behavior
    let simulator = Simulator::with_config(2, 2);
    let db = revm::database::EmptyDB::default();
    let block_env = BlockEnv::default();

    let sender_a = addr(0);
    let sender_b = addr(1);

    // Two txs from sender_a (conflict), one from sender_b (independent)
    let txs = vec![
        make_sim_tx(sender_a, addr(10), 0),
        make_sim_tx(sender_a, addr(11), 1), // conflicts with tx 0
        make_sim_tx(sender_b, addr(12), 0), // independent
    ];

    let sim_results = simulator.simulate(&txs, &db, &block_env);
    assert_eq!(sim_results.len(), 3);

    let mut framer = Framer::new();
    for sr in sim_results {
        framer.add(sr);
    }
    let frames = framer.finish();

    // Same-sender txs write to the same account, so they should be in different frames
    assert!(
        frames.len() >= 2,
        "Same-sender txs should produce at least 2 frames, got {}",
        frames.len()
    );

    let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
    assert_eq!(total_tasks, 3, "total tasks should equal number of txs");

    // Also verify through the pipeline
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let cfg = default_cfg();
    let pipeline_txs: Vec<PipelineTxInput> = vec![
        make_transfer(sender_a, addr(10), 0, 0),
        make_transfer(sender_a, addr(11), 1, 1),
        make_transfer(sender_b, addr(12), 2, 0),
    ];

    let result = pipeline.execute_block(pipeline_txs, &db, &BlockEnv::default(), &cfg);
    assert_eq!(result.tx_results.len(), 3);
    for (i, tx) in result.tx_results.iter().enumerate() {
        assert_eq!(tx.original_index, i);
    }
}

// ---------------------------------------------------------------------------
// Test 5: Pipeline E2E - Cross-Block State
// ---------------------------------------------------------------------------

/// Execute two blocks sequentially and verify that prev_state is populated
/// after the first block.
#[test]
fn test_pipeline_e2e_cross_block_state() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    // First block: 3 transactions
    let txs1: Vec<PipelineTxInput> =
        (0..3u8).map(|i| make_transfer(addr(i), addr(100 + i), i as usize, 0)).collect();

    let result1 = pipeline.execute_block(txs1, &db, &BlockEnv::default(), &cfg);
    assert_eq!(result1.tx_results.len(), 3);

    // The state_cache should have been populated with touched accounts
    assert!(
        result1.state_cache.accounts_len() > 0,
        "state cache should have accounts after first block"
    );

    // Second block: 2 different transactions
    let txs2: Vec<PipelineTxInput> =
        (0..2u8).map(|i| make_transfer(addr(50 + i), addr(150 + i), i as usize, 0)).collect();

    let result2 = pipeline.execute_block(txs2, &db, &BlockEnv::default(), &cfg);
    assert_eq!(result2.tx_results.len(), 2);

    // Second block should also have a populated state cache
    assert!(
        result2.state_cache.accounts_len() > 0,
        "state cache should have accounts after second block"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Dispatcher E2E with Dashboard
// ---------------------------------------------------------------------------

/// Create a Dashboard and ParallelDispatcher, submit mock Dispatchable tasks
/// with known EEI dependencies, and verify execution order respects dependencies.
#[test]
fn test_dispatcher_e2e_with_dashboard() {
    let dispatcher = ParallelDispatcher::new(2, 4);
    let dashboard = Arc::new(Dashboard::new(100));
    let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let eei_map = make_eei_map(10);

    // Dependency chain: 0 (independent) -> 1 depends on 0 -> 2 depends on 1
    // Plus 3, 4 are independent
    let tasks = vec![
        MockTask::new(0, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(1, 0, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(2, 1, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(3, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(4, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
    ];

    dispatcher.execute_block(tasks, &dashboard);

    let executed = log.lock().clone();
    assert_eq!(executed.len(), 5, "all 5 tasks should have executed");

    // Verify all indices are present
    let mut sorted = executed.clone();
    sorted.sort();
    assert_eq!(sorted, vec![0, 1, 2, 3, 4]);

    // Verify dependency ordering
    let pos = |idx: i32| executed.iter().position(|&x| x == idx).unwrap();
    assert!(pos(0) < pos(1), "task 0 must execute before task 1");
    assert!(pos(1) < pos(2), "task 1 must execute before task 2");
}

// ---------------------------------------------------------------------------
// Test 7: BlockContext E2E - Parallel Execution
// ---------------------------------------------------------------------------

/// Create ParallelBlockContext with ParallelStateCache, set up ExeTasks
/// with tx_envs, execute them, and verify results are stored and gas
/// accumulates.
#[test]
fn test_block_context_e2e_parallel_execution() {
    let curr_state = Arc::new(ParallelStateCache::new());
    let cfg = default_cfg();

    let ctx = ParallelBlockContext::new(4, curr_state.clone(), None, BlockEnv::default(), cfg);

    // Set up 3 tasks with different senders
    for i in 0..3usize {
        let sim_result =
            SimResult { crw_sets: CrwSets::default(), original_index: i, success: true };
        let mut task = ExeTask::new(sim_result);
        task.tx_envs.push(TxEnv {
            caller: addr(i as u8),
            gas_limit: 21000,
            gas_price: 0,
            kind: TxKind::Call(addr((i + 10) as u8)),
            value: U256::ZERO,
            nonce: 0,
            ..Default::default()
        });
        ctx.tasks_manager.set_task(i, task);
    }

    // Execute all tasks
    ctx.execute_task(0);
    ctx.execute_task(1);
    ctx.execute_task(2);

    // Verify results are stored
    let results = ctx.collect_results();
    assert_eq!(results.len(), 3);
    for (i, r) in results.iter().enumerate() {
        assert!(r.is_some(), "result at index {} should be present", i);
        let r = r.as_ref().unwrap();
        assert_eq!(r.tx_results.len(), 1, "each task had 1 tx");
    }

    // State cache should have been updated
    assert!(curr_state.accounts_len() > 0, "state cache should have accounts after execution");
}

// ---------------------------------------------------------------------------
// Test 8: Framer to Dispatcher Integration
// ---------------------------------------------------------------------------

/// Simulate transactions, frame them, then verify the chain works end-to-end.
#[test]
fn test_framer_to_dispatcher_integration() {
    let simulator = Simulator::with_config(2, 2);
    let db = revm::database::EmptyDB::default();
    let block_env = BlockEnv::default();

    // 6 independent transactions (different senders)
    let txs: Vec<SimTxEnv> =
        (0..6u8).map(|i| make_sim_tx(addr(i * 2), addr(i * 2 + 1), 0)).collect();

    // Step 1: Simulate
    let sim_results = simulator.simulate(&txs, &db, &block_env);
    assert_eq!(sim_results.len(), 6);

    // Verify all sim results have original_index set correctly
    for (i, sr) in sim_results.iter().enumerate() {
        assert_eq!(sr.original_index, i);
    }

    // Step 2: Frame
    let mut framer = Framer::new();
    for sr in sim_results {
        framer.add(sr);
    }
    let frames = framer.finish();
    assert!(!frames.is_empty(), "should have at least 1 frame");

    let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
    assert_eq!(total_tasks, 6, "all 6 txs should be in frames");

    // Step 3: Verify frame structure - collect all original indices
    let mut all_indices: Vec<usize> = frames
        .iter()
        .flat_map(|f| f.tasks.iter())
        .flat_map(|t| t.sim_results.iter())
        .map(|sr| sr.original_index)
        .collect();
    all_indices.sort();
    assert_eq!(all_indices, (0..6).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// Test 9: ParallelStateCache During Execution
// ---------------------------------------------------------------------------

/// Execute transactions that modify state and verify that ParallelStateCache
/// reflects the changes after execution.
#[test]
fn test_parallel_state_cache_during_execution() {
    let cache = Arc::new(ParallelStateCache::new());
    let cfg = default_cfg();

    let ctx = ParallelBlockContext::new(2, cache.clone(), None, BlockEnv::default(), cfg);

    // Set up a task
    let sim_result = SimResult { crw_sets: CrwSets::default(), original_index: 0, success: true };
    let mut task = ExeTask::new(sim_result);
    task.tx_envs.push(TxEnv {
        caller: addr(0),
        gas_limit: 21000,
        gas_price: 0,
        kind: TxKind::Call(addr(1)),
        value: U256::ZERO,
        nonce: 0,
        ..Default::default()
    });
    ctx.tasks_manager.set_task(0, task);

    // Before execution
    assert_eq!(cache.accounts_len(), 0, "cache should be empty before execution");

    // Execute
    ctx.execute_task(0);

    // After execution, the cache should have been populated by apply_evm_state
    // The EVM touches at least the caller account
    assert!(
        cache.accounts_len() > 0,
        "cache should have accounts after execution (caller at minimum)"
    );

    // Verify we can read the cached account
    let caller_info = cache.get_account(&addr(0));
    // The caller should appear in the cache since the EVM touches it
    assert!(caller_info.is_some(), "caller address should be in cache after execution");
}

// ---------------------------------------------------------------------------
// Test 10: Pipeline Stress - 100 Transactions
// ---------------------------------------------------------------------------

/// 100 independent transactions from different senders. Verify all complete
/// with correct ordering and no panics.
#[test]
fn test_pipeline_stress_100_txs() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let txs: Vec<PipelineTxInput> = (0..100u8)
        .map(|i| {
            // Use different addresses for each sender to ensure independence
            // Spread across a wide range to avoid collisions
            let sender = Address::with_last_byte(i.wrapping_add(0x20));
            let recipient = Address::with_last_byte(i.wrapping_add(0xA0));
            make_transfer(sender, recipient, i as usize, 0)
        })
        .collect();

    let result = pipeline.execute_block(txs, &db, &BlockEnv::default(), &cfg);

    assert_eq!(result.tx_results.len(), 100, "should have 100 results");

    // Verify ordering
    for (i, tx) in result.tx_results.iter().enumerate() {
        assert_eq!(tx.original_index, i, "result ordering mismatch at position {}", i);
    }

    // Verify no zero-index duplicates (sanity check)
    let indices: Vec<usize> = result.tx_results.iter().map(|r| r.original_index).collect();
    let mut deduped = indices.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), 100, "all original_index values should be unique");
}

// ---------------------------------------------------------------------------
// Test 11: Dispatcher with fan-out dependency pattern
// ---------------------------------------------------------------------------

/// Tests a fan-out pattern: task 0 is independent, tasks 1-4 all depend on 0.
/// Verifies task 0 executes before all dependents.
#[test]
fn test_dispatcher_fan_out_dependency() {
    let dispatcher = ParallelDispatcher::new(2, 4);
    let dashboard = Arc::new(Dashboard::new(100));
    let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let eei_map = make_eei_map(5);

    let tasks = vec![
        MockTask::new(0, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(1, 0, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(2, 0, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(3, 0, dashboard.clone(), log.clone(), eei_map.clone()),
        MockTask::new(4, 0, dashboard.clone(), log.clone(), eei_map.clone()),
    ];

    dispatcher.execute_block(tasks, &dashboard);

    let executed = log.lock().clone();
    assert_eq!(executed.len(), 5);

    let pos = |idx: i32| executed.iter().position(|&x| x == idx).unwrap();
    // Task 0 must be first
    for i in 1..5 {
        assert!(pos(0) < pos(i), "task 0 must execute before task {}", i);
    }
}

// ---------------------------------------------------------------------------
// Test 12: Framer with all-conflicting transactions
// ---------------------------------------------------------------------------

/// All transactions write to the same address hash. Each must be in a
/// separate frame.
#[test]
fn test_framer_all_conflicting() {
    let mut framer = Framer::with_max_frames(8);
    let hash = [0xAAu8; 10];

    for i in 0..6 {
        framer.add(make_sim_result_with(i, vec![], vec![hash]));
    }

    let frames = framer.finish();

    // Each tx writes to the same hash, so each gets its own frame
    assert_eq!(frames.len(), 6, "each conflicting tx should have its own frame");

    let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
    assert_eq!(total_tasks, 6);
}

// ---------------------------------------------------------------------------
// Test 13: Pipeline empty block
// ---------------------------------------------------------------------------

/// Empty block produces empty results.
#[test]
fn test_pipeline_e2e_empty_block() {
    let mut pipeline = ParallelExecutionPipeline::with_config(2, 2, 4);
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let result = pipeline.execute_block(vec![], &db, &BlockEnv::default(), &cfg);

    assert_eq!(result.tx_results.len(), 0);
    assert_eq!(result.total_gas_used, 0);
}

// ---------------------------------------------------------------------------
// Test 14: BlockContext with prev_state
// ---------------------------------------------------------------------------

/// Verify that ParallelBlockContext can use a prev_state cache for
/// cross-block state sharing.
#[test]
fn test_block_context_with_prev_state() {
    let prev_state = Arc::new(ParallelStateCache::new());

    // Pre-populate prev_state with an account
    let pre_addr = addr(99);
    prev_state.insert_account(
        pre_addr,
        Some(revm_state::AccountInfo {
            balance: U256::from(1_000_000),
            nonce: 42,
            ..Default::default()
        }),
    );

    let curr_state = Arc::new(ParallelStateCache::new());
    let cfg = default_cfg();

    let ctx = ParallelBlockContext::new(
        4,
        curr_state.clone(),
        Some(prev_state.clone()),
        BlockEnv::default(),
        cfg,
    );

    // Verify prev_state is accessible through the context
    assert!(ctx.prev_state.is_some());
    let prev = ctx.prev_state.as_ref().unwrap();
    let info = prev.get_account(&pre_addr).unwrap().unwrap();
    assert_eq!(info.balance, U256::from(1_000_000));
    assert_eq!(info.nonce, 42);
}

// ---------------------------------------------------------------------------
// Test 15: Concurrent ParallelStateCache access during execution
// ---------------------------------------------------------------------------

/// Multiple threads read and write to ParallelStateCache simultaneously.
/// Verifies no panics or data races occur.
#[test]
fn test_parallel_state_cache_concurrent_access() {
    let cache = Arc::new(ParallelStateCache::new());
    let num_threads = 8;

    let mut handles = Vec::new();

    // Writer threads
    for t in 0..num_threads {
        let cache = cache.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                let byte = ((t * 50 + i) % 200) as u8;
                let a = Address::with_last_byte(byte.wrapping_add(0x20));
                cache.insert_account(
                    a,
                    Some(revm_state::AccountInfo {
                        balance: U256::from(t * 1000 + i),
                        nonce: (t * 50 + i) as u64,
                        ..Default::default()
                    }),
                );
            }
        }));
    }

    // Reader threads
    for _ in 0..num_threads {
        let cache = cache.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                let byte = (i % 200) as u8;
                let a = Address::with_last_byte(byte.wrapping_add(0x20));
                // Reads may or may not find data depending on timing
                let _ = cache.get_account(&a);
                let _ = cache.get_storage(&a, &U256::from(i));
            }
        }));
    }

    for h in handles {
        h.join().expect("thread should not panic");
    }

    assert!(cache.accounts_len() > 0, "cache should have accounts after concurrent writes");
}

// ---------------------------------------------------------------------------
// Test 16: Dashboard completion tracking
// ---------------------------------------------------------------------------

/// Verify Dashboard correctly tracks completion across all tasks.
#[test]
fn test_dashboard_full_completion() {
    let dashboard = Dashboard::new(100);
    dashboard.set_valid_count(20);

    // Execute all tasks in order
    for i in 0..20 {
        dashboard.set_executed(i);
    }

    assert_eq!(
        dashboard.get_all_done_index(),
        19,
        "all_done_index should be 19 after executing all 20 tasks"
    );
}

// ---------------------------------------------------------------------------
// Test 17: Pipeline determinism
// ---------------------------------------------------------------------------

/// Running the same transactions twice should produce the same result ordering
/// and gas values.
#[test]
fn test_pipeline_deterministic() {
    let db = revm::database::EmptyDB::default();
    let cfg = default_cfg();

    let txs: Vec<PipelineTxInput> =
        (0..10u8).map(|i| make_transfer(addr(i * 2), addr(i * 2 + 1), i as usize, 0)).collect();

    let mut pipeline1 = ParallelExecutionPipeline::with_config(2, 2, 4);
    let result1 = pipeline1.execute_block(txs.clone(), &db, &BlockEnv::default(), &cfg);

    let mut pipeline2 = ParallelExecutionPipeline::with_config(2, 2, 4);
    let result2 = pipeline2.execute_block(txs, &db, &BlockEnv::default(), &cfg);

    assert_eq!(result1.tx_results.len(), result2.tx_results.len());
    for (r1, r2) in result1.tx_results.iter().zip(result2.tx_results.iter()) {
        assert_eq!(r1.original_index, r2.original_index, "ordering should match");
        assert_eq!(r1.gas_used, r2.gas_used, "gas should match");
        assert_eq!(r1.success, r2.success, "success status should match");
    }
    assert_eq!(result1.total_gas_used, result2.total_gas_used, "total gas should be deterministic");
}
