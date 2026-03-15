//! Global registry for pre-warming cache access
//!
//! This provides a static accessor for the pre-warmed cache, allowing
//! the payload builder to access pre-warmed keys without needing to
//! pass the pool through complex trait bounds.
//!
//! ## Usage
//!
//! In transaction pool (when initializing):
//! ```ignore
//! let cache = Arc::new(PreWarmedCache::new(config));
//! reth_transaction_pool::pre_warming::registry::set_global_cache(cache.clone());
//! ```
//!
//! In payload builder:
//! ```ignore
//! if let Some(cache) = reth_transaction_pool::pre_warming::registry::get_global_cache() {
//!     let keys = cache.get_all_keys();
//!     // prefetch...
//! }
//! ```

use crate::pre_warming::{PreWarmedCache, PreWarmingMetrics};
use alloy_primitives::{TxHash, B256};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, OnceLock,
};

use super::snapshot_state::SnapshotState;

/// Global cache holder
static GLOBAL_CACHE: RwLock<Option<Arc<PreWarmedCache>>> = RwLock::new(None);

/// Global metrics holder
static GLOBAL_METRICS: RwLock<Option<Arc<PreWarmingMetrics>>> = RwLock::new(None);

/// Global prefetch threads count (defaults to available CPUs)
static GLOBAL_PREFETCH_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Warm simulation snapshot shared with the payload builder.
///
/// Simulation workers populate this snapshot's DashMap cache as they process
/// mempool transactions. The payload builder reuses it instead of creating a
/// fresh cold SnapshotState, converting MDBX queries into in-memory DashMap hits.
static GLOBAL_SIMULATION_SNAPSHOT: RwLock<Option<Arc<SnapshotState>>> = RwLock::new(None);

/// Set while the payload builder is actively building a block.
///
/// Simulation drain_loop checks this flag before acquiring a simulation permit.
/// When true, the drain loop skips new simulations and yields until the flag
/// clears — typically 100-200ms. This eliminates the ~11ms CPU competition
/// penalty measured when 4 simulation workers compete with block execution.
static BLOCK_BUILDING: AtomicBool = AtomicBool::new(false);

/// Signal that block building has started; simulation workers will pause.
pub fn set_block_building(building: bool) {
    BLOCK_BUILDING.store(building, Ordering::Relaxed);
}

/// Returns true if the payload builder is currently inside a block build.
pub fn is_block_building() -> bool {
    BLOCK_BUILDING.load(Ordering::Relaxed)
}

/// Global TX arrival time tracker.
///
/// Maps tx_hash → Instant the TX first entered the pool.
/// Used to compute time-to-inclusion (pool arrival → sealed in block).
/// Entries are removed when the TX is included in a block or evicted from the pool.
static GLOBAL_TX_ARRIVAL_TIMES: OnceLock<Arc<DashMap<TxHash, std::time::Instant>>> =
    OnceLock::new();

/// Initialise the TX arrival time tracker. Called once during pool setup.
pub fn init_tx_arrival_tracker() {
    let _ = GLOBAL_TX_ARRIVAL_TIMES.set(Arc::new(DashMap::new()));
}

/// Record the arrival time for a transaction entering the pool.
///
/// Uses `entry().or_insert_with()` so re-submissions don't overwrite the original time.
pub fn record_tx_arrival(tx_hash: TxHash) {
    if let Some(map) = GLOBAL_TX_ARRIVAL_TIMES.get() {
        map.entry(tx_hash).or_insert_with(std::time::Instant::now);
    }
}

/// Remove and return the arrival time for a transaction included in a block.
pub fn take_tx_arrival_time(tx_hash: &TxHash) -> Option<std::time::Instant> {
    GLOBAL_TX_ARRIVAL_TIMES.get()?.remove(tx_hash).map(|(_, t)| t)
}

/// Evict the arrival time entry when a TX is dropped or replaced from the pool.
pub fn evict_tx_arrival_time(tx_hash: &TxHash) {
    if let Some(map) = GLOBAL_TX_ARRIVAL_TIMES.get() {
        map.remove(tx_hash);
    }
}

/// Parent block hash and cache size at the time of the last prefetch.
///
/// `build_payload` is called every ~200ms per slot. We skip re-prefetch if
/// the parent block has not changed AND the cache has not grown significantly
/// since the last run. This avoids redundant work while still re-prefetching
/// when simulation workers have finished processing a new batch of transactions.
static LAST_PREFETCH_STATE: RwLock<Option<(B256, usize)>> = RwLock::new(None);

/// Set the global pre-warmed cache
///
/// Called by the transaction pool when the worker pool is initialized.
pub fn set_global_cache(cache: Arc<PreWarmedCache>) {
    let ptr = Arc::as_ptr(&cache);
    *GLOBAL_CACHE.write() = Some(cache);
    tracing::warn!(
        target: "txpool::pre_warming",
        cache_ptr = ?ptr,
        ">>> set_global_cache called"
    );
}

/// Set the global pre-warming metrics
///
/// Called by the transaction pool when the worker pool is initialized.
pub fn set_global_metrics(metrics: Arc<PreWarmingMetrics>) {
    *GLOBAL_METRICS.write() = Some(metrics);
    tracing::warn!(
        target: "txpool::pre_warming",
        ">>> set_global_metrics called"
    );
}

/// Get the global pre-warmed cache (if registered)
///
/// Returns None if pre-warming is not initialized.
pub fn get_global_cache() -> Option<Arc<PreWarmedCache>> {
    GLOBAL_CACHE.read().clone()
}

/// Get the global pre-warming metrics (if registered)
///
/// Returns None if pre-warming is not initialized.
pub fn get_global_metrics() -> Option<Arc<PreWarmingMetrics>> {
    GLOBAL_METRICS.read().clone()
}

/// Set the global prefetch threads count
///
/// Called by the transaction pool when the worker pool is initialized.
pub fn set_global_prefetch_threads(num_threads: usize) {
    GLOBAL_PREFETCH_THREADS.store(num_threads, Ordering::Relaxed);
    tracing::warn!(
        target: "txpool::pre_warming",
        num_threads,
        ">>> set_global_prefetch_threads called"
    );
}

/// Get the global prefetch threads count
///
/// Returns the configured number of prefetch threads, or defaults to available CPUs.
pub fn get_global_prefetch_threads() -> usize {
    let stored = GLOBAL_PREFETCH_THREADS.load(Ordering::Relaxed);
    if stored == 0 {
        // Default to available CPUs if not set
        std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4)
    } else {
        stored
    }
}

/// Check if pre-warming is active (cache registered)
pub fn is_pre_warming_active() -> bool {
    GLOBAL_CACHE.read().is_some()
}

/// Clear the global cache (for testing or shutdown)
pub fn clear_global_cache() {
    *GLOBAL_CACHE.write() = None;
}

/// Register the simulation workers' warm snapshot for payload builder reuse.
///
/// Called by `SimulationWorkerPool::update_snapshot` on every canonical block change.
/// The snapshot's DashMap cache accumulates queried state across simulations,
/// so the payload builder can reuse it instead of opening a fresh cold MDBX transaction.
pub fn set_global_simulation_snapshot(snapshot: Arc<SnapshotState>) {
    *GLOBAL_SIMULATION_SNAPSHOT.write() = Some(snapshot);
}

/// Get the simulation workers' warm snapshot (if registered).
///
/// Returns `None` if the worker pool has not initialized yet.
pub fn get_global_simulation_snapshot() -> Option<Arc<SnapshotState>> {
    GLOBAL_SIMULATION_SNAPSHOT.read().clone()
}

/// Number of new cache entries required before re-running prefetch for the same block.
///
/// When `build_payload` first fires for a new parent, simulation workers may have
/// processed only a handful of pending transactions. Setting this threshold allows
/// the payload builder to re-prefetch as more simulations complete, ensuring
/// CachedReads is warm before the block deadline.
const REPREFETCH_GROWTH_THRESHOLD: usize = 200;

/// Returns true if `build_payload` should run prefetch for `parent_hash`.
///
/// Prefetch runs on the first call for a new parent block. It also re-runs for
/// the same parent if the PreWarmedCache has grown by at least
/// `REPREFETCH_GROWTH_THRESHOLD` entries since the last prefetch, which happens
/// as simulation workers finish processing the incoming transaction stream.
pub fn should_prefetch_for_parent(parent_hash: B256, current_cache_size: usize) -> bool {
    // Fast path: read lock only.
    {
        let state = LAST_PREFETCH_STATE.read();
        if let Some((last_hash, last_size)) = *state {
            if last_hash == parent_hash {
                let growth = current_cache_size.saturating_sub(last_size);
                if growth < REPREFETCH_GROWTH_THRESHOLD {
                    return false; // Same block, not enough new entries simulated yet
                }
            }
        }
    }
    // Parent changed or cache grew enough — acquire write lock to update.
    let mut state = LAST_PREFETCH_STATE.write();
    // Double-check after acquiring write lock.
    if let Some((last_hash, last_size)) = *state {
        if last_hash == parent_hash {
            let growth = current_cache_size.saturating_sub(last_size);
            if growth < REPREFETCH_GROWTH_THRESHOLD {
                return false;
            }
        }
    }
    *state = Some((parent_hash, current_cache_size));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_warming::PreWarmingConfig;

    #[test]
    fn test_global_cache_registration() {
        // Initially not active
        assert!(!is_pre_warming_active());
        assert!(get_global_cache().is_none());

        // Register cache
        let config = PreWarmingConfig::default();
        let cache = Arc::new(PreWarmedCache::new(config));
        set_global_cache(cache);

        // Now active
        assert!(is_pre_warming_active());
        assert!(get_global_cache().is_some());

        // Clear
        clear_global_cache();
        assert!(!is_pre_warming_active());
    }
}
