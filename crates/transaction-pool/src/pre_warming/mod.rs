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
//! - **CachedReads** (EXISTING): Stores actual data (already in Reth at `crates/revm/src/cached.rs`)
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

pub mod cache;
pub mod config;
pub mod types;
pub mod worker_pool;
mod simulator;
mod snapshot_state;
pub mod bridge;
pub mod metrics;

#[cfg(test)]
mod tests;

pub use cache::{CacheStats, PreWarmedCache};
pub use config::PreWarmingConfig;
pub use types::{ExtractedKeys, SimulationRequest};
pub use worker_pool::SimulationWorkerPool;
pub use simulator::Simulator;
pub use snapshot_state::SnapshotState;
pub use bridge::{prefetch_and_populate, prefetch_parallel, prefetch_with_snapshot, populate_cached_reads_from_keys, get_cache_stats};
pub use metrics::PreWarmingMetrics;


