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
use alloy_primitives::B256;
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
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

/// Parent block hash for which prefetch was last run.
///
/// `build_payload` is called every ~200ms per slot. We only prefetch once per
/// parent block — subsequent calls reuse the already-warm CachedReads.
static LAST_PREFETCHED_PARENT: RwLock<Option<B256>> = RwLock::new(None);

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

/// Returns true if `build_payload` should run prefetch for `parent_hash`.
///
/// The payload builder is called every ~200ms per slot with the same parent hash.
/// Only the first call needs to prefetch — subsequent calls already have warm
/// CachedReads from the first call. Returns false on repeated calls for the same
/// parent, skipping redundant MDBX queries and thread creation.
pub fn should_prefetch_for_parent(parent_hash: B256) -> bool {
    // Fast path: read lock only (no write needed on repeated calls for same parent).
    if LAST_PREFETCHED_PARENT.read().as_ref() == Some(&parent_hash) {
        return false;
    }
    // Parent changed — acquire write lock to update.
    let mut last = LAST_PREFETCHED_PARENT.write();
    if *last == Some(parent_hash) {
        return false; // Another thread raced and already set it
    }
    *last = Some(parent_hash);
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
