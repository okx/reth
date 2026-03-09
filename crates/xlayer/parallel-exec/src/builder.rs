//! Parallel block builder orchestrating the full execution pipeline.
//!
//! Pipeline: Simulator -> Framer -> Dispatcher -> ResultCollector
//!
//! The builder owns the [`Simulator`] and [`Dispatcher`], reusing them
//! across blocks. A fresh [`ParallelStateCache`] is created per block to
//! avoid stale intra-block state leaking across boundaries.

use crate::{
    dispatcher::{Dispatcher, TxExecutionResult},
    framer::Framer,
    result_collector,
    simulator::{SimTxEnv, Simulator},
    state_cache::ParallelStateCache,
};
use alloy_primitives::U256;
use revm::context::BlockEnv;

/// Result of building a block with parallel execution.
pub struct ParallelBlockResult {
    /// Ordered execution results for each transaction.
    pub tx_results: Vec<TxExecutionResult>,
    /// Merged EVM state from all transactions.
    pub merged_state: revm::state::EvmState,
    /// Total gas used by all transactions.
    pub total_gas_used: u64,
    /// Total fees collected (placeholder until per-tx effective gas price is available).
    pub total_fees: U256,
}

impl core::fmt::Debug for ParallelBlockResult {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParallelBlockResult")
            .field("tx_count", &self.tx_results.len())
            .field("total_gas_used", &self.total_gas_used)
            .field("merged_accounts", &self.merged_state.len())
            .finish()
    }
}

/// Orchestrates parallel block building.
///
/// Owns the [`Simulator`] and [`Dispatcher`], reusing them across blocks.
/// The state cache is created per-block to ensure isolation.
pub struct ParallelBlockBuilder {
    simulator: Simulator,
    dispatcher: Dispatcher,
}

impl core::fmt::Debug for ParallelBlockBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParallelBlockBuilder")
            .field("simulator_shards", &self.simulator.shard_count())
            .field("dispatcher_threads", &self.dispatcher.thread_count())
            .finish()
    }
}

impl ParallelBlockBuilder {
    /// Create a new builder with default settings (4 simulator shards, 64 execution threads).
    pub fn new() -> Self {
        Self { simulator: Simulator::new(), dispatcher: Dispatcher::new(64) }
    }

    /// Create a builder with custom settings.
    pub fn with_config(sim_shards: usize, exec_threads: usize) -> Self {
        Self {
            simulator: Simulator::with_shard_count(sim_shards),
            dispatcher: Dispatcher::new(exec_threads),
        }
    }

    /// Build a block by executing transactions in parallel.
    ///
    /// Pipeline:
    /// 1. **Simulator**: pre-execute txs to extract CrwSets
    /// 2. **Framer**: group non-conflicting txs into frames
    /// 3. **Dispatcher**: execute frames serially with intra-frame parallelism
    /// 4. **ResultCollector**: merge results into ordered output
    pub fn build(
        &self,
        txs: Vec<SimTxEnv>,
        fallback: &(dyn reth_storage_api::StateProvider + Sync),
        block_env: &BlockEnv,
    ) -> ParallelBlockResult {
        // Create per-block state cache
        let cache = ParallelStateCache::new();

        // 1. Simulate to extract CrwSets
        let cached_provider = crate::state_cache::CachedStateProvider::new(&cache, fallback);
        let sim_results = self.simulator.simulate(&txs, &cached_provider, block_env);

        // 2. Frame non-conflicting transactions
        let mut framer = Framer::new();
        for sim_result in sim_results {
            framer.add(sim_result);
        }
        let frames = framer.finish();

        // 3. Execute frames with intra-frame parallelism
        let raw_results = self.dispatcher.execute(frames, &cache, fallback, block_env, &txs);

        // 4. Collect and merge results
        let ordered_results = result_collector::collect_ordered_results(raw_results);
        let merged_state = result_collector::merge_states(&ordered_results);
        let (total_gas_used, total_fees) =
            result_collector::compute_gas_and_fees(&ordered_results, None);

        ParallelBlockResult {
            tx_results: ordered_results,
            merged_state,
            total_gas_used,
            total_fees,
        }
    }
}

impl Default for ParallelBlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_creation() {
        let builder = ParallelBlockBuilder::new();
        assert_eq!(builder.simulator.shard_count(), 4);
        assert_eq!(builder.dispatcher.thread_count(), 64);
    }

    #[test]
    fn test_builder_with_config() {
        let builder = ParallelBlockBuilder::with_config(8, 16);
        assert_eq!(builder.simulator.shard_count(), 8);
        assert_eq!(builder.dispatcher.thread_count(), 16);
    }

    #[test]
    fn test_builder_default() {
        let builder = ParallelBlockBuilder::default();
        assert_eq!(builder.simulator.shard_count(), 4);
        assert_eq!(builder.dispatcher.thread_count(), 64);
    }

    #[test]
    fn test_builder_debug() {
        let builder = ParallelBlockBuilder::new();
        let debug_str = format!("{:?}", builder);
        assert!(debug_str.contains("ParallelBlockBuilder"));
    }

    #[test]
    fn test_parallel_block_result_debug() {
        let result = ParallelBlockResult {
            tx_results: vec![],
            merged_state: Default::default(),
            total_gas_used: 0,
            total_fees: U256::ZERO,
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("ParallelBlockResult"));
        assert!(debug_str.contains("tx_count"));
    }
}
