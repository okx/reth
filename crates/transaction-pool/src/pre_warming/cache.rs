//! Cache for pre-warmed keys
//!
//! PreWarmedCache aggregates ExtractedKeys from ALL simulated transactions.
//! In Phase 2, the block builder queries this cache once to get ALL keys,
//! then batch-fetches everything from MDBX before execution starts.
//!
//! ## Why Aggregate Instead of Per-Transaction?
//!
//! Block builders don't select transactions upfront - they pull from an iterator
//! and decide whether to include each transaction based on execution results.
//! Since we can't predict which transactions will be included, we aggregate
//! ALL keys and pre-fetch everything.

use crate::pre_warming::{ExtractedKeys, PreWarmingConfig};
use parking_lot::RwLock;
use std::time::Instant;

/// Aggregated keys with metadata
#[derive(Debug, Clone)]
struct AggregatedKeys {
    /// Aggregated keys from all simulations
    keys: ExtractedKeys,

    /// When this aggregate was last refreshed
    last_refresh: Instant,

    /// Number of simulations merged since last refresh
    simulation_count: usize,
}

impl AggregatedKeys {
    fn new() -> Self {
        Self {
            keys: ExtractedKeys::new(),
            last_refresh: Instant::now(),
            simulation_count: 0,
        }
    }

    /// Check if this aggregate is stale
    fn is_stale(&self, ttl: std::time::Duration) -> bool {
        self.last_refresh.elapsed() > ttl
    }
}

/// Thread-safe cache for aggregated pre-warmed keys
///
/// This cache aggregates keys from ALL simulated transactions into a single
/// ExtractedKeys structure. The block builder queries this once to get all keys,
/// then batch-fetches everything from MDBX before execution.
///
/// ## Usage Flow:
/// ```text
/// Simulation workers:
///   tx1 simulated → merge_keys(keys1)
///   tx2 simulated → merge_keys(keys2)
///   tx3 simulated → merge_keys(keys3)
///   ...
///
/// Block builder:
///   all_keys = cache.get_all_keys()
///   batch_fetch_from_mdbx(all_keys)
///   pre_populate_cached_reads()
///   execute_transactions()  // All cache hits!
/// ```
#[derive(Debug)]
pub struct PreWarmedCache {
    /// Aggregated keys from all simulations
    aggregated: RwLock<AggregatedKeys>,

    /// Configuration (for TTL)
    config: PreWarmingConfig,
}

impl PreWarmedCache {
    /// Create new cache with given configuration
    pub fn new(config: PreWarmingConfig) -> Self {
        Self {
            aggregated: RwLock::new(AggregatedKeys::new()),
            config,
        }
    }

    /// Merge keys from a simulation into the aggregate
    ///
    /// This is called by simulation workers after extracting keys.
    /// Keys are automatically deduplicated via HashSet.
    pub fn merge_keys(&self, keys: ExtractedKeys) {
        let mut aggregated = self.aggregated.write();
        aggregated.keys.merge(keys);
        aggregated.simulation_count += 1;
    }

    /// Get all aggregated keys for batch fetching
    ///
    /// This is called by the block builder before execution starts.
    /// Returns a clone of all aggregated keys.
    pub fn get_all_keys(&self) -> ExtractedKeys {
        self.aggregated.read().keys.clone()
    }

    /// Refresh the cache if stale
    ///
    /// Clears aggregated keys if they're older than TTL.
    /// Should be called periodically (e.g., on new block).
    pub fn refresh_if_stale(&self) -> bool {
        let mut aggregated = self.aggregated.write();

        if aggregated.is_stale(self.config.cache_ttl) {
            *aggregated = AggregatedKeys::new();
            true
        } else {
            false
        }
    }

    /// Force refresh (clear all aggregated keys)
    pub fn refresh(&self) {
        let mut aggregated = self.aggregated.write();
        *aggregated = AggregatedKeys::new();
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let aggregated = self.aggregated.read();

        CacheStats {
            total_accounts: aggregated.keys.accounts.len(),
            total_storage_slots: aggregated.keys.storage_slots.len(),
            total_code_hashes: aggregated.keys.code_hashes.len(),
            total_block_hashes: aggregated.keys.block_hashes.len(),
            total_keys: aggregated.keys.total_keys(),
            simulation_count: aggregated.simulation_count,
            age_seconds: aggregated.last_refresh.elapsed().as_secs(),
            is_stale: aggregated.is_stale(self.config.cache_ttl),
        }
    }

    /// Clear all aggregated keys
    pub fn clear(&self) {
        self.refresh();
    }
}

/// Cache statistics
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    /// Total unique accounts
    pub total_accounts: usize,

    /// Total unique storage slots
    pub total_storage_slots: usize,

    /// Total unique code hashes
    pub total_code_hashes: usize,

    /// Total unique block hashes
    pub total_block_hashes: usize,

    /// Total keys (sum of above)
    pub total_keys: usize,

    /// Number of simulations merged
    pub simulation_count: usize,

    /// Age of aggregate in seconds
    pub age_seconds: u64,

    /// Whether aggregate is stale
    pub is_stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use std::time::Duration;

    fn test_config() -> PreWarmingConfig {
        PreWarmingConfig {
            enabled: true,
            cache_ttl: Duration::from_millis(100),
            ..PreWarmingConfig::default()
        }
    }

    // ============================================================================
    // Basic Functionality Tests
    // ============================================================================

    #[test]
    fn test_merge_keys() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));

        cache.merge_keys(keys1);
        cache.merge_keys(keys2);

        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 2);
    }

    #[test]
    fn test_get_all_keys_empty() {
        let cache = PreWarmedCache::new(test_config());
        let all_keys = cache.get_all_keys();

        assert!(all_keys.is_empty());
        assert_eq!(all_keys.total_keys(), 0);
    }

    #[test]
    fn test_deduplication() {
        let cache = PreWarmedCache::new(test_config());

        let addr = Address::from([1; 20]);

        // Add same address multiple times
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(addr);

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(addr);

        cache.merge_keys(keys1);
        cache.merge_keys(keys2);

        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 1);  // Deduplicated!
    }

    // ============================================================================
    // Refresh and Staleness Tests
    // ============================================================================

    #[test]
    fn test_refresh_if_stale() {
        let cache = PreWarmedCache::new(test_config());

        // Add some keys
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        // Should have keys
        assert!(!cache.get_all_keys().is_empty());

        // Wait for staleness
        std::thread::sleep(Duration::from_millis(150));

        // Refresh
        let refreshed = cache.refresh_if_stale();
        assert!(refreshed);

        // Should be empty now
        assert!(cache.get_all_keys().is_empty());
    }

    #[test]
    fn test_refresh_if_not_stale() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        // Immediately check (not stale)
        let refreshed = cache.refresh_if_stale();
        assert!(!refreshed);

        // Should still have keys
        assert!(!cache.get_all_keys().is_empty());
    }

    #[test]
    fn test_force_refresh() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        assert!(!cache.get_all_keys().is_empty());

        cache.refresh();

        assert!(cache.get_all_keys().is_empty());
    }

    #[test]
    fn test_clear_same_as_refresh() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        cache.clear();

        assert!(cache.get_all_keys().is_empty());
    }

    // ============================================================================
    // Statistics Tests
    // ============================================================================

    #[test]
    fn test_stats() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        keys.add_account(Address::random());
        keys.add_storage_slot(Address::random(), U256::from(1));

        cache.merge_keys(keys.clone());
        cache.merge_keys(keys);

        let stats = cache.stats();
        assert_eq!(stats.total_accounts, 2);
        assert_eq!(stats.total_storage_slots, 1);
        assert_eq!(stats.total_keys, 3);
        assert_eq!(stats.simulation_count, 2);
    }

    #[test]
    fn test_stats_empty_cache() {
        let cache = PreWarmedCache::new(test_config());
        let stats = cache.stats();

        assert_eq!(stats.total_keys, 0);
        assert_eq!(stats.simulation_count, 0);
        assert_eq!(stats.total_accounts, 0);
    }

    #[test]
    fn test_stats_after_refresh() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        cache.refresh();

        let stats = cache.stats();
        assert_eq!(stats.total_keys, 0);
        assert_eq!(stats.simulation_count, 0);
    }

    #[test]
    fn test_stats_staleness_flag() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.merge_keys(keys);

        let stats1 = cache.stats();
        assert!(!stats1.is_stale);

        std::thread::sleep(Duration::from_millis(150));

        let stats2 = cache.stats();
        assert!(stats2.is_stale);
    }

    // ============================================================================
    // Concurrent Access Tests
    // ============================================================================

    #[test]
    fn test_concurrent_merge() {
        use std::sync::Arc;

        let cache = Arc::new(PreWarmedCache::new(test_config()));
        let mut handles = vec![];

        // Spawn 4 threads merging concurrently
        for i in 0..4 {
            let cache = cache.clone();
            let handle = std::thread::spawn(move || {
                for j in 0..10 {
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(Address::from([(i * 10 + j) as u8; 20]));
                    cache.merge_keys(keys);
                }
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have aggregated all keys
        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 40);  // 4 threads × 10 unique addresses
    }

    #[test]
    fn test_concurrent_merge_and_get() {
        use std::sync::Arc;

        let cache = Arc::new(PreWarmedCache::new(test_config()));
        let mut handles = vec![];

        // Spawn writers
        for i in 0..2 {
            let cache = cache.clone();
            let handle = std::thread::spawn(move || {
                for j in 0..100 {
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(Address::from([(i * 100 + j) as u8; 20]));
                    cache.merge_keys(keys);
                }
            });
            handles.push(handle);
        }

        // Spawn readers
        for _ in 0..2 {
            let cache = cache.clone();
            let handle = std::thread::spawn(move || {
                for _ in 0..100 {
                    let _all_keys = cache.get_all_keys();
                }
            });
            handles.push(handle);
        }

        // Wait for all
        for handle in handles {
            handle.join().unwrap();
        }

        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 200);
    }

    #[test]
    fn test_concurrent_refresh_and_merge() {
        use std::sync::Arc;

        let cache = Arc::new(PreWarmedCache::new(test_config()));
        let mut handles = vec![];

        // Constant merging
        for i in 0..4 {
            let cache = cache.clone();
            let handle = std::thread::spawn(move || {
                for j in 0..50 {
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(Address::from([(i * 50 + j) as u8; 20]));
                    cache.merge_keys(keys);
                    std::thread::sleep(Duration::from_micros(10));
                }
            });
            handles.push(handle);
        }

        // Periodic refreshing
        let cache_clone = cache.clone();
        let refresh_handle = std::thread::spawn(move || {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(5));
                cache_clone.refresh();
            }
        });
        handles.push(refresh_handle);

        for handle in handles {
            handle.join().unwrap();
        }

        // Should complete without panic
    }

    // ============================================================================
    // Large Scale Tests
    // ============================================================================

    #[test]
    fn test_massive_merge_operations() {
        let cache = PreWarmedCache::new(test_config());

        // Merge 10,000 ExtractedKeys
        for i in 0..10_000 {
            let mut keys = ExtractedKeys::new();
            keys.add_account(Address::from([i as u8; 20]));
            cache.merge_keys(keys);
        }

        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 256); // Limited by u8 range

        let stats = cache.stats();
        assert_eq!(stats.simulation_count, 10_000);
    }

    #[test]
    fn test_large_aggregated_keys() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys = ExtractedKeys::new();

        // Add 1000 accounts
        for i in 0..1000 {
            let mut bytes = [0u8; 20];
            bytes[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            keys.add_account(Address::from(bytes));
        }

        // Add 1000 storage slots
        for i in 0..1000 {
            keys.add_storage_slot(Address::random(), U256::from(i));
        }

        cache.merge_keys(keys);

        let all_keys = cache.get_all_keys();
        assert_eq!(all_keys.accounts.len(), 1000);
        assert_eq!(all_keys.storage_slots.len(), 1000);
        assert_eq!(all_keys.total_keys(), 2000);
    }

    #[test]
    fn test_merge_then_get_performance() {
        let cache = PreWarmedCache::new(test_config());

        // Merge many keys
        for i in 0..1000 {
            let mut keys = ExtractedKeys::new();
            keys.add_account(Address::from([i as u8; 20]));
            keys.add_storage_slot(Address::random(), U256::from(i));
            cache.merge_keys(keys);
        }

        // Getting all keys should be fast (just a clone)
        let start = std::time::Instant::now();
        let _all_keys = cache.get_all_keys();
        let elapsed = start.elapsed();

        // Should be very fast (< 10ms for cloning)
        assert!(elapsed.as_millis() < 10);
    }

    // ============================================================================
    // Edge Cases
    // ============================================================================

    #[test]
    fn test_merge_empty_keys() {
        let cache = PreWarmedCache::new(test_config());

        let empty_keys = ExtractedKeys::new();
        cache.merge_keys(empty_keys);

        let all_keys = cache.get_all_keys();
        assert!(all_keys.is_empty());

        let stats = cache.stats();
        assert_eq!(stats.simulation_count, 1); // Still counts as a simulation
    }

    #[test]
    fn test_get_all_keys_is_snapshot() {
        let cache = PreWarmedCache::new(test_config());

        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));
        cache.merge_keys(keys1);

        let snapshot = cache.get_all_keys();

        // Add more after getting snapshot
        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));
        cache.merge_keys(keys2);

        // Snapshot shouldn't change
        assert_eq!(snapshot.accounts.len(), 1);

        // But new get should have both
        let new_snapshot = cache.get_all_keys();
        assert_eq!(new_snapshot.accounts.len(), 2);
    }

    #[test]
    fn test_refresh_resets_simulation_count() {
        let cache = PreWarmedCache::new(test_config());

        cache.merge_keys(ExtractedKeys::new());
        cache.merge_keys(ExtractedKeys::new());

        let stats1 = cache.stats();
        assert_eq!(stats1.simulation_count, 2);

        cache.refresh();

        let stats2 = cache.stats();
        assert_eq!(stats2.simulation_count, 0);
    }

    #[test]
    fn test_age_increases_over_time() {
        let cache = PreWarmedCache::new(test_config());
        cache.merge_keys(ExtractedKeys::new());

        let stats1 = cache.stats();
        let age1 = stats1.age_seconds;

        std::thread::sleep(Duration::from_millis(100));

        let stats2 = cache.stats();
        let age2 = stats2.age_seconds;

        assert!(age2 >= age1);
    }

    // ============================================================================
    // TTL Configuration Tests
    // ============================================================================

    #[test]
    fn test_different_ttl_configurations() {
        // Short TTL
        let config_short = PreWarmingConfig {
            cache_ttl: Duration::from_millis(50),
            ..test_config()
        };
        let cache = PreWarmedCache::new(config_short);

        cache.merge_keys(ExtractedKeys::new());

        std::thread::sleep(Duration::from_millis(60));

        let stats = cache.stats();
        assert!(stats.is_stale);
    }

    #[test]
    fn test_long_ttl_not_stale() {
        let config_long = PreWarmingConfig {
            cache_ttl: Duration::from_secs(3600),
            ..test_config()
        };
        let cache = PreWarmedCache::new(config_long);

        cache.merge_keys(ExtractedKeys::new());

        std::thread::sleep(Duration::from_millis(100));

        let stats = cache.stats();
        assert!(!stats.is_stale);
    }

    // ============================================================================
    // Real-World Simulation Tests
    // ============================================================================

    #[test]
    fn test_realistic_block_building_scenario() {
        let cache = PreWarmedCache::new(test_config());

        // Simulate 50 transactions being simulated
        for i in 0..50 {
            let mut keys = ExtractedKeys::new();
            keys.add_account(Address::from([i; 20]));
            keys.add_storage_slot(Address::from([i; 20]), U256::from(i as u64));
            cache.merge_keys(keys);
        }

        // Block builder gets all keys
        let all_keys = cache.get_all_keys();

        // Should have aggregated everything
        assert_eq!(all_keys.accounts.len(), 50);
        assert_eq!(all_keys.storage_slots.len(), 50);

        // After block built, refresh for next block
        cache.refresh();

        // Should be empty again
        assert!(cache.get_all_keys().is_empty());
    }

    #[test]
    fn test_continuous_simulation_flow() {
        let cache = PreWarmedCache::new(PreWarmingConfig {
            cache_ttl: Duration::from_secs(10),
            ..test_config()
        });

        // Simulate continuous transaction arrival
        for round in 0..5 {
            // Each round simulates 20 transactions
            for i in 0..20 {
                let mut keys = ExtractedKeys::new();
                let idx = round * 20 + i;
                keys.add_account(Address::from([idx as u8; 20]));
                cache.merge_keys(keys);
            }

            // Periodic check
            std::thread::sleep(Duration::from_millis(10));
        }

        let stats = cache.stats();
        assert_eq!(stats.simulation_count, 100);
    }
}



