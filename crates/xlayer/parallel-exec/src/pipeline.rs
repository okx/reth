//! Parallel execution pipeline orchestrator.
//!
//! Ties together: Simulator -> Framer -> Dispatcher -> Result collection
//!
//! Pipeline flow:
//! 1. Simulator pre-executes transactions to extract CrwSets
//! 2. Framer groups non-conflicting transactions into frames using ParaBloom
//! 3. Dispatcher executes tasks in parallel with dependency tracking
//! 4. Results are collected in original transaction order

use crate::{
    dispatcher_new::ParallelDispatcher,
    framer::Framer,
    parallel_state_cache::ParallelStateCache,
    simulator::{SimTxEnv, Simulator},
};
use alloy_primitives::Address;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use std::sync::Arc;

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

/// Parallel execution pipeline.
///
/// Reusable across blocks. Owns the Simulator and Dispatcher thread pools.
///
/// MVP strategy: frames execute serially, tasks within each frame execute
/// in parallel on the Dispatcher's rayon pool. The full Dashboard-based
/// cascade dispatch will be wired up in a future iteration.
pub struct ParallelExecutionPipeline {
    /// Simulator for pre-execution (CrwSets extraction).
    simulator: Simulator,
    /// Dispatcher for parallel execution.
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
    /// Pipeline:
    /// 1. Convert inputs to SimTxEnvs
    /// 2. Simulate to extract CrwSets (parallel, using Simulator)
    /// 3. Frame non-conflicting transactions (sequential, using Framer+ParaBloom)
    /// 4. Execute frames serially, tasks within each frame in parallel
    /// 5. Collect results in original transaction order
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

        // 4. Execute frames serially, tasks within frames in parallel.
        // This is the MVP execution strategy: frame-serial + intra-frame-parallel.
        let curr_state = Arc::new(ParallelStateCache::new());
        let mut all_results: Vec<PipelineTxResult> = Vec::with_capacity(txs.len());
        let mut total_gas = 0u64;

        for frame in frames {
            let frame_results: Vec<Vec<PipelineTxResult>> =
                self.dispatcher.exe_pool_install(|| {
                    use rayon::prelude::*;
                    frame
                        .tasks
                        .par_iter()
                        .map(|task| {
                            let mut task_results = Vec::new();
                            for sim_result in &task.sim_results {
                                let tx_env = sim_txs[sim_result.original_index].tx_env.clone();

                                let result = crate::execute::execute_tx_with_ref(
                                    db, block_env, cfg_env, tx_env,
                                );

                                task_results.push(PipelineTxResult {
                                    original_index: sim_result.original_index,
                                    result: result.result,
                                    state: result.state,
                                    gas_used: result.gas_used,
                                    success: result.success,
                                });
                            }
                            task_results
                        })
                        .collect()
                });

            // Apply state changes from this frame to cache
            for task_results in &frame_results {
                for tx_result in task_results {
                    curr_state.apply_evm_state(&tx_result.state);
                    total_gas = total_gas.saturating_add(tx_result.gas_used);
                }
            }

            for task_results in frame_results {
                all_results.extend(task_results);
            }
        }

        // Sort by original index to preserve block ordering
        all_results.sort_by_key(|r| r.original_index);

        // Update prev_state for next block
        self.prev_state = Some(curr_state.clone());

        PipelineBlockResult {
            tx_results: all_results,
            total_gas_used: total_gas,
            state_cache: curr_state,
        }
    }
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
}
