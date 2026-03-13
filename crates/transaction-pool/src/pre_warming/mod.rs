//! Pre-warming simulation for transaction pool
//!
//! This module implements a parallel simulation system that:
//! 1. Simulates transactions in background workers after they're added to the pool
//! 2. Extracts KEYS (not values!) that will be accessed during execution
//! 3. Stores these keys in PreWarmedCache
//! 4. In Phase 2, these keys will be used to batch-fetch from MDBX and pre-populate CachedReads
//!
//! ## Architecture
//!
//! Two-cache system:
//! - **PreWarmedCache** (NEW): Stores keys only (what we build here)
//! - **CachedReads** (EXISTING): Stores actual data (already in Reth at
//!   `crates/revm/src/cached.rs`)
//!
//! ## Flow
//!
//! Phase 1 (this module):
//! ```text
//! Transaction added to pool
//!   ↓
//! Trigger simulation (fire-and-forget)
//!   ↓
//! Worker simulates and extracts KEYS
//!   ↓
//! Store in PreWarmedCache
//! ```
//!
//! Phase 2 (future work in block builder):
//! ```text
//! Query PreWarmedCache for selected transactions
//!   ↓
//! Aggregate keys
//!   ↓
//! Batch fetch from MDBX
//!   ↓
//! Pre-populate CachedReads
//!   ↓
//! Execute (all cache hits!)
//! ```

pub mod bridge;
pub mod cache;
pub mod config;
pub mod metrics;
pub mod registry;
mod simulator;
mod snapshot_state;
pub mod types;
pub mod worker_pool;

#[cfg(test)]
mod tests;

pub use bridge::{
    get_cache_stats, prefetch_with_arcs_sync, prefetch_with_snapshot, prefetch_with_snapshot_sync,
};
pub use cache::{CacheStats, PreWarmedCache};
pub use config::PreWarmingConfig;
pub use metrics::PreWarmingMetrics;
pub use registry::{
    clear_global_cache, get_global_cache, get_global_metrics, get_global_prefetch_threads,
    is_pre_warming_active, set_global_cache, set_global_metrics, set_global_prefetch_threads,
};
pub use simulator::Simulator;
pub use snapshot_state::SnapshotState;
pub use types::{ExtractedKeys, SimulationRequest};
pub use worker_pool::SimulationWorkerPool;

/// Trait for transaction pools that support pre-warming via simulation.
///
/// This trait provides access to pre-warmed keys discovered by background simulation.
/// The keys can be used to prefetch state before block execution.
pub trait PreWarmingPool {
    /// Get ALL pre-warmed keys discovered by background simulation.
    ///
    /// Returns merged ExtractedKeys for all cached transactions.
    /// Returns `None` if pre-warming is not active.
    fn get_all_prewarmed_keys(&self) -> Option<ExtractedKeys>;

    /// Check if pre-warming is active (worker pool initialized).
    fn is_pre_warming_active(&self) -> bool;

    /// Get the number of threads to use for parallel prefetch.
    ///
    /// Defaults to the number of simulation workers configured.
    fn prefetch_threads(&self) -> usize {
        4 // Default fallback
    }
}

/// Default implementation for unit type when no pool is provided.
impl PreWarmingPool for () {
    fn get_all_prewarmed_keys(&self) -> Option<ExtractedKeys> {
        None
    }

    fn is_pre_warming_active(&self) -> bool {
        false
    }

    fn prefetch_threads(&self) -> usize {
        4
    }
}
