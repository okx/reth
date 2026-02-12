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

use crate::pre_warming::PreWarmedCache;
use parking_lot::RwLock;
use std::sync::Arc;

/// Global cache holder
static GLOBAL_CACHE: RwLock<Option<Arc<PreWarmedCache>>> = RwLock::new(None);

/// Set the global pre-warmed cache
///
/// Called by the transaction pool when the worker pool is initialized.
pub fn set_global_cache(cache: Arc<PreWarmedCache>) {
    *GLOBAL_CACHE.write() = Some(cache);
    tracing::debug!(
        target: "txpool::pre_warming",
        "Global pre-warming cache registered"
    );
}

/// Get the global pre-warmed cache (if registered)
///
/// Returns None if pre-warming is not initialized.
pub fn get_global_cache() -> Option<Arc<PreWarmedCache>> {
    GLOBAL_CACHE.read().clone()
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

