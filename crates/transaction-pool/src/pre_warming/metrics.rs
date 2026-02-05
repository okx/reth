//! Pre-warming simulation metrics.
//!
//! This module provides Prometheus metrics for monitoring the pre-warming
//! simulation system. These metrics help operators understand:
//!
//! - How many simulations are being performed
//! - Success/failure rates
//! - Cache effectiveness
//! - Prefetch performance
//!
//! # Metric Categories
//!
//! ## Simulation Metrics
//! Track the simulation worker pool performance:
//! - `simulations_triggered` - Total simulations requested
//! - `simulations_completed` - Successfully completed simulations
//! - `simulations_failed` - Failed simulations (timeout, error)
//! - `simulations_dropped` - Dropped due to channel full (backpressure)
//! - `simulation_duration` - Histogram of simulation times
//!
//! ## Cache Metrics
//! Track the pre-warmed cache effectiveness:
//! - `cache_entries` - Current number of cached transaction keys
//! - `cache_keys_total` - Total keys across all cached transactions
//! - `cache_hits` - Keys found during prefetch
//! - `cache_misses` - Keys not found (TX not simulated)
//! - `cache_evictions` - Keys removed (TX mined or dropped)
//!
//! ## Prefetch Metrics
//! Track the prefetch phase performance:
//! - `prefetch_accounts` - Accounts prefetched from MDBX
//! - `prefetch_storage_slots` - Storage slots prefetched
//! - `prefetch_contracts` - Bytecode prefetched
//! - `prefetch_duration` - Time spent prefetching
//!
//! # Example
//!
//! ```ignore
//! // In worker_pool.rs when simulation completes:
//! PRE_WARMING_METRICS.simulations_completed.increment(1);
//! PRE_WARMING_METRICS.simulation_duration.record(duration.as_secs_f64());
//! ```

use reth_metrics::{
    metrics::{Counter, Gauge, Histogram},
    Metrics,
};

/// Pre-warming simulation metrics.
///
/// These metrics are registered under the `txpool_pre_warming` scope in Prometheus.
///
/// # Prometheus Metric Names
///
/// All metrics are prefixed with `txpool_pre_warming_`:
/// - `txpool_pre_warming_simulations_triggered`
/// - `txpool_pre_warming_simulations_completed`
/// - etc.
#[derive(Metrics)]
#[metrics(scope = "txpool_pre_warming")]
pub struct PreWarmingMetrics {
    // ========================================================================
    // Simulation Worker Pool Metrics
    // ========================================================================

    /// Total number of simulations triggered (requested).
    ///
    /// Incremented when `trigger_simulation()` is called, regardless of
    /// whether the simulation is queued or dropped.
    pub simulations_triggered: Counter,

    /// Number of simulations successfully completed.
    ///
    /// Incremented when a worker finishes simulating a transaction and
    /// successfully extracts keys.
    pub simulations_completed: Counter,

    /// Number of simulations that failed.
    ///
    /// Includes timeouts, EVM errors, and other failures.
    /// Does NOT include dropped simulations (see `simulations_dropped`).
    pub simulations_failed: Counter,

    /// Number of simulations dropped due to backpressure.
    ///
    /// When the bounded channel is full, new simulation requests are dropped.
    /// High values indicate workers can't keep up with transaction rate.
    /// Consider increasing `num_workers` if this is high.
    pub simulations_dropped: Counter,

    /// Histogram of simulation duration in seconds.
    ///
    /// Tracks how long each simulation takes. Useful for:
    /// - Identifying slow simulations
    /// - Tuning simulation timeout
    /// - Capacity planning
    pub simulation_duration: Histogram,

    // ========================================================================
    // Cache Metrics
    // ========================================================================

    /// Current number of transactions in the cache.
    ///
    /// Each transaction has its own set of extracted keys.
    /// This gauge shows how many transactions have cached keys.
    pub cache_entries: Gauge,

    /// Total number of keys across all cached transactions.
    ///
    /// Sum of accounts + storage slots + code hashes + block hashes
    /// across all cached transactions.
    pub cache_keys_total: Gauge,

    /// Number of cache hits during key retrieval.
    ///
    /// Incremented when `get_keys_for_txs()` finds a requested TX in cache.
    pub cache_hits: Counter,

    /// Number of cache misses during key retrieval.
    ///
    /// Incremented when `get_keys_for_txs()` doesn't find a requested TX.
    /// High misses indicate TXs are being selected before simulation completes.
    pub cache_misses: Counter,

    /// Number of transactions evicted from cache.
    ///
    /// Incremented when mined/dropped TXs are removed via `remove_txs()`.
    pub cache_evictions: Counter,

    // ========================================================================
    // Prefetch Metrics
    // ========================================================================

    /// Number of accounts prefetched from MDBX.
    ///
    /// Incremented during the prefetch phase for each account loaded.
    pub prefetch_accounts: Counter,

    /// Number of storage slots prefetched from MDBX.
    ///
    /// Incremented during the prefetch phase for each storage slot loaded.
    pub prefetch_storage_slots: Counter,

    /// Number of contracts (bytecode) prefetched from MDBX.
    ///
    /// Incremented during the prefetch phase for each contract loaded.
    pub prefetch_contracts: Counter,

    /// Histogram of prefetch duration in seconds.
    ///
    /// Tracks how long the parallel prefetch phase takes.
    /// Should be significantly faster than sequential MDBX queries during execution.
    pub prefetch_duration: Histogram,

    /// Number of prefetch operations performed.
    ///
    /// Incremented each time `prefetch_with_snapshot()` is called.
    pub prefetch_operations: Counter,

    // ========================================================================
    // Snapshot Metrics
    // ========================================================================

    /// Number of snapshot updates.
    ///
    /// Incremented when `update_pre_warming_snapshot()` is called.
    /// Should roughly match the number of new blocks.
    pub snapshot_updates: Counter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        // Verify metrics can be created without panicking
        // The #[derive(Metrics)] macro creates Default implementation
        // that registers metrics with the global Prometheus registry
        let metrics = PreWarmingMetrics::default();
        
        // Verify we can increment counters
        metrics.simulations_triggered.increment(1);
        metrics.simulations_completed.increment(1);
        metrics.simulations_failed.increment(1);
        metrics.simulations_dropped.increment(1);
        
        // Verify we can record histograms
        metrics.simulation_duration.record(0.015);
        metrics.prefetch_duration.record(0.005);
        
        // Verify we can set gauges
        metrics.cache_entries.set(100);
        metrics.cache_keys_total.set(5000);
    }
}

