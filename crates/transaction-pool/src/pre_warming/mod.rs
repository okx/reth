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

mod cache;
mod config;
mod types;

pub use cache::{CacheStats, PreWarmedCache};
pub use config::PreWarmingConfig;
pub use types::{ExtractedKeys, SimulationRequest};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxHash};
    
    #[test]
    fn test_module_exports() {
        // Verify all types are accessible
        let _config = PreWarmingConfig::default();
        let _cache = PreWarmedCache::new(PreWarmingConfig::default());
        let _keys = ExtractedKeys::new();
        let _request = SimulationRequest::new(TxHash::random(), 42u64);
    }
    
    #[test]
    fn test_end_to_end_aggregated_flow() {
        // Simulate complete flow: extract keys → merge → retrieve all
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);
        
        // Simulate extracting keys from multiple transactions
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::random());
        keys1.add_account(Address::random());
        
        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::random());
        keys2.add_storage_slot(Address::random(), alloy_primitives::U256::from(5));
        
        // Merge keys from both simulations
        cache.merge_keys(keys1);
        cache.merge_keys(keys2);
        
        // Retrieve all aggregated keys
        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 3);
        assert_eq!(all_keys.storage_slots.len(), 1);
    }
}

