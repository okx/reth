//! Block timing metrics for tracking block production and execution times

use alloy_primitives::B256;
use indexmap::IndexMap;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub use crate::block_timing_prometheus::BlockTimingPrometheusMetrics;

/// Timing metrics for block building phase
#[derive(Debug, Clone, Default)]
pub struct BuildTiming {
    /// Time spent applying pre-execution changes
    pub apply_pre_execution_changes: Duration,
    /// Time spent executing sequencer transactions
    pub exec_sequencer_transactions: Duration,
    /// Time spent selecting/packing mempool transactions
    pub select_mempool_transactions: Duration,
    /// Time spent executing mempool transactions
    pub exec_mempool_transactions: Duration,
    /// Time spent calculating state root
    pub calc_state_root: Duration,
    /// Total build time
    pub total: Duration,
}

/// Timing metrics for block insertion phase
#[derive(Debug, Clone, Default)]
pub struct InsertTiming {
    /// Time spent validating and executing the block
    pub validate_and_execute: Duration,
    /// Time spent inserting to tree state
    pub insert_to_tree: Duration,
    /// Total insert time
    pub total: Duration,
}

/// Complete timing metrics for a block
#[derive(Debug, Clone, Default)]
pub struct BlockTimingMetrics {
    /// Block building phase timing
    pub build: BuildTiming,
    /// Block insertion phase timing
    pub insert: InsertTiming,
}

impl BlockTimingMetrics {
    /// Format timing metrics for logging
    pub fn format_for_log(&self) -> String {
        let format_duration = |d: Duration| {
            let secs = d.as_secs_f64();
            match () {
                _ if secs >= 1.0 => format!("{:.3}s", secs),
                _ if secs >= 0.001 => {
                    let ms = secs * 1000.0;
                    if ms.fract() == 0.0 {
                        format!("{}ms", ms as u64)
                    } else {
                        format!("{:.3}ms", ms)
                    }
                }
                _ if secs >= 0.000001 => {
                    let us = secs * 1_000_000.0;
                    if us.fract() == 0.0 {
                        format!("{}µs", us as u64)
                    } else {
                        format!("{:.3}µs", us)
                    }
                }
                _ => format!("{}ns", d.as_nanos()),
            }
        };

        // Check if block was built locally (has build timing) or received from network
        let is_locally_built = self.build.total.as_nanos() > 0;

        if is_locally_built {
            // Block was built locally, show full timing including Build
            // Note: Transaction execution times (execSeqTxs, selectMempoolTxs, execMempoolTxs) are
            // already shown in Build phase
            format!(
                "Produce[Build[applyPreExec<{}>, execSeqTxs<{}>, selectMempoolTxs<{}>, execMempoolTxs<{}>, calcStateRoot<{}>, total<{}>], Insert[validateExec<{}>, insertTree<{}>, total<{}>]]",
                format_duration(self.build.apply_pre_execution_changes),
                format_duration(self.build.exec_sequencer_transactions),
                format_duration(self.build.select_mempool_transactions),
                format_duration(self.build.exec_mempool_transactions),
                format_duration(self.build.calc_state_root),
                format_duration(self.build.total),
                format_duration(self.insert.validate_and_execute),
                format_duration(self.insert.insert_to_tree),
                format_duration(self.insert.total),
            )
        } else {
            // Block was received from network, only show Insert timing
            format!(
                "Produce[Insert[validateExec<{}>, insertTree<{}>, total<{}>]]",
                format_duration(self.insert.validate_and_execute),
                format_duration(self.insert.insert_to_tree),
                format_duration(self.insert.total),
            )
        }
    }
}

/// Global storage for block timing metrics
///
/// Uses `IndexMap` to maintain insertion order, allowing us to remove the oldest entries
/// when the cache exceeds the limit.
static BLOCK_TIMING_STORE: std::sync::OnceLock<Arc<Mutex<IndexMap<B256, BlockTimingMetrics>>>> =
    std::sync::OnceLock::new();

/// Initialize the global block timing store
fn get_timing_store() -> Arc<Mutex<IndexMap<B256, BlockTimingMetrics>>> {
    BLOCK_TIMING_STORE.get_or_init(|| Arc::new(Mutex::new(IndexMap::new()))).clone()
}

/// Store timing metrics for a block
///
/// If the block already exists, it will be updated and moved to the end (most recent).
/// When the cache exceeds 1000 entries, the oldest entries are removed.
pub fn store_block_timing(block_hash: B256, metrics: BlockTimingMetrics) {
    let store = get_timing_store();
    let mut map = store.lock().unwrap();

    // If the block already exists, remove it first so it can be re-inserted at the end
    // This ensures that updated blocks are treated as the most recent
    if map.contains_key(&block_hash) {
        map.shift_remove(&block_hash);
    }

    // Insert at the end (most recent position)
    map.insert(block_hash, metrics);

    // Clean up old entries to prevent memory leak (keep last 1000 blocks)
    // IndexMap maintains insertion order, so we can safely remove from the front
    const MAX_ENTRIES: usize = 1000;
    while map.len() > MAX_ENTRIES {
        // Remove the oldest entry (first in insertion order)
        map.shift_remove_index(0);
    }
}

/// Retrieve timing metrics for a block
pub fn get_block_timing(block_hash: &B256) -> Option<BlockTimingMetrics> {
    let store = get_timing_store();
    let map = store.lock().unwrap();
    map.get(block_hash).cloned()
}

/// Remove timing metrics for a block (after logging)
pub fn remove_block_timing(block_hash: &B256) {
    let store = get_timing_store();
    let mut map = store.lock().unwrap();
    map.shift_remove(block_hash);
}

// ============================================================================
// RAII-based timing helpers
// ============================================================================

/// RAII guard that records elapsed time to a [`Duration`] field and a Prometheus histogram on drop.
#[derive(Debug)]
pub struct TimingGuard<'a> {
    start: Instant,
    target: &'a mut Duration,
    prometheus_histogram: &'a metrics::Histogram,
}

impl<'a> TimingGuard<'a> {
    /// Create a new timing guard that records to both `target` and Prometheus histogram.
    pub fn new(target: &'a mut Duration, prometheus_histogram: &'a metrics::Histogram) -> Self {
        Self { start: Instant::now(), target, prometheus_histogram }
    }
}

impl Drop for TimingGuard<'_> {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        *self.target = duration;
        self.prometheus_histogram.record(duration.as_secs_f64());
    }
}

/// Context for managing block timing metrics using RAII.
///
/// This provides a cleaner API for recording timing metrics throughout the block
/// building and insertion process. The context automatically stores metrics when dropped.
///
/// Supports both logging (via `IndexMap`) and Prometheus metrics recording.
#[derive(Debug)]
pub struct BlockTimingContext {
    block_hash: B256,
    metrics: BlockTimingMetrics,
    prometheus_metrics: BlockTimingPrometheusMetrics,
    /// Tracks whether any build-phase guard was created in this context.
    timed_build: bool,
    /// Tracks whether any insert-phase guard was created in this context.
    timed_insert: bool,
}

impl BlockTimingContext {
    /// Create a new timing context for a block.
    ///
    /// If timing metrics already exist for this block (e.g., from build phase),
    /// they will be loaded. Otherwise, a new empty metrics structure is created.
    ///
    /// Metrics will be automatically stored when the context is dropped.
    pub fn new(block_hash: B256, prometheus_metrics: BlockTimingPrometheusMetrics) -> Self {
        Self {
            block_hash,
            metrics: get_block_timing(&block_hash).unwrap_or_default(),
            prometheus_metrics,
            timed_build: false,
            timed_insert: false,
        }
    }

    /// Create a new timing context for a block, initializing with empty metrics.
    ///
    /// Metrics will be automatically stored when the context is dropped.
    pub fn new_empty(block_hash: B256, prometheus_metrics: BlockTimingPrometheusMetrics) -> Self {
        Self {
            block_hash,
            metrics: BlockTimingMetrics::default(),
            prometheus_metrics,
            timed_build: false,
            timed_insert: false,
        }
    }

    /// Update the block hash for this context.
    pub fn set_block_hash(&mut self, block_hash: B256) {
        self.block_hash = block_hash;
    }

    /// Create a timing guard for recording build phase: apply pre-execution changes.
    pub fn time_apply_pre_execution_changes(&mut self) -> TimingGuard<'_> {
        self.timed_build = true;
        TimingGuard::new(
            &mut self.metrics.build.apply_pre_execution_changes,
            &self.prometheus_metrics.build_apply_pre_execution_changes,
        )
    }

    /// Create a timing guard for recording build phase: execute sequencer transactions.
    pub fn time_exec_sequencer_transactions(&mut self) -> TimingGuard<'_> {
        self.timed_build = true;
        TimingGuard::new(
            &mut self.metrics.build.exec_sequencer_transactions,
            &self.prometheus_metrics.build_exec_sequencer_transactions,
        )
    }

    /// Create a timing guard for recording build phase: select/pack mempool transactions.
    pub fn time_select_mempool_transactions(&mut self) -> TimingGuard<'_> {
        self.timed_build = true;
        TimingGuard::new(
            &mut self.metrics.build.select_mempool_transactions,
            &self.prometheus_metrics.build_select_mempool_transactions,
        )
    }

    /// Create a timing guard for recording build phase: execute mempool transactions.
    pub fn time_exec_mempool_transactions(&mut self) -> TimingGuard<'_> {
        self.timed_build = true;
        TimingGuard::new(
            &mut self.metrics.build.exec_mempool_transactions,
            &self.prometheus_metrics.build_exec_mempool_transactions,
        )
    }

    /// Create a timing guard for recording build phase: calculate state root.
    pub fn time_calc_state_root(&mut self) -> TimingGuard<'_> {
        self.timed_build = true;
        TimingGuard::new(
            &mut self.metrics.build.calc_state_root,
            &self.prometheus_metrics.build_calc_state_root,
        )
    }

    /// Create a timing guard for recording insert phase: validate and execute.
    pub fn time_validate_and_execute(&mut self) -> TimingGuard<'_> {
        self.timed_insert = true;
        TimingGuard::new(
            &mut self.metrics.insert.validate_and_execute,
            &self.prometheus_metrics.insert_validate_and_execute,
        )
    }

    /// Create a timing guard for recording insert phase: insert to tree.
    pub fn time_insert_to_tree(&mut self) -> TimingGuard<'_> {
        self.timed_insert = true;
        TimingGuard::new(
            &mut self.metrics.insert.insert_to_tree,
            &self.prometheus_metrics.insert_insert_to_tree,
        )
    }

    /// Store the current metrics to the global store.
    fn store(&self) {
        store_block_timing(self.block_hash, self.metrics.clone());
    }

    /// Calculate total build time from individual components.
    fn calculate_build_total(&self) -> Duration {
        self.metrics.build.apply_pre_execution_changes +
            self.metrics.build.exec_sequencer_transactions +
            self.metrics.build.select_mempool_transactions +
            self.metrics.build.exec_mempool_transactions +
            self.metrics.build.calc_state_root
    }

    /// Calculate total insert time from individual components.
    fn calculate_insert_total(&self) -> Duration {
        self.metrics.insert.validate_and_execute + self.metrics.insert.insert_to_tree
    }

    /// Calculate and update total times, recording only phases that were actively
    /// timed in this context to Prometheus.
    ///
    /// This prevents double-recording: e.g. the INSERT context loads BUILD data from
    /// the global store but should not re-record `build_total` to Prometheus.
    fn update_totals(&mut self) {
        self.metrics.build.total = self.calculate_build_total();
        self.metrics.insert.total = self.calculate_insert_total();

        if self.timed_build && !self.metrics.build.total.is_zero() {
            self.prometheus_metrics.build_total.record(self.metrics.build.total.as_secs_f64());
        }
        if self.timed_insert && !self.metrics.insert.total.is_zero() {
            self.prometheus_metrics.insert_total.record(self.metrics.insert.total.as_secs_f64());
        }
    }
}

impl Drop for BlockTimingContext {
    fn drop(&mut self) {
        self.update_totals();
        self.store();
    }
}
