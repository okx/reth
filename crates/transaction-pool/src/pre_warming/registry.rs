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
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// Global cache holder
static GLOBAL_CACHE: RwLock<Option<Arc<PreWarmedCache>>> = RwLock::new(None);

/// Global metrics holder
static GLOBAL_METRICS: RwLock<Option<Arc<PreWarmingMetrics>>> = RwLock::new(None);

/// Global prefetch threads count (defaults to available CPUs)
static GLOBAL_PREFETCH_THREADS: AtomicUsize = AtomicUsize::new(0);

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
