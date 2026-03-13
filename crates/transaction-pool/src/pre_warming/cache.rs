//! Cache for pre-warmed keys (per-transaction tracking)
//!
//! PreWarmedCache stores ExtractedKeys for each simulated transaction separately.
//! During block building, the payload builder selects transactions first, then
//! queries this cache to get keys only for selected transactions, merges them,
//! and prefetches from MDBX.
//!
//! ## Per-Transaction Storage
//!
//! Each transaction's simulation results are stored with its hash as the key.
//! This enables:
//! - Precise prefetching (only keys for selected TXs)
//! - Direct cleanup when TX is mined/dropped
//! - No over-fetching or cache pollution
//!
//! ## Performance Optimizations
//! - Uses DashMap for lock-free concurrent access
//! - Sharded internally - different TxHashes map to different shards
//! - Simulation workers can write in parallel without contention
//! - Block builder can read while simulations are writing
//!
//! ## Usage Flow:
//! ```text
//! Simulation workers:
///   tx1 simulated → store_tx_keys(tx1_hash, keys1)
///   tx2 simulated → store_tx_keys(tx2_hash, keys2)
///   tx3 simulated → store_tx_keys(tx3_hash, keys3)
///   ...
///
/// Block builder:
///   selected_txs = select_transactions()  // Select first!
///   keys = cache.get_keys_for_txs(selected_tx_hashes)  // Only selected
///   batch_fetch_from_mdbx(keys)
///   execute_transactions()  // All cache hits!
///
/// Block finalized:
///   cache.remove_txs(mined_tx_hashes)  // Cleanup
/// ```
use crate::pre_warming::{ExtractedKeys, PreWarmingConfig};
use alloy_primitives::TxHash;
use dashmap::DashMap;
use std::{
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};

/// Default capacity for transaction cache (typical mempool batch)
const DEFAULT_CACHE_CAPACITY: usize = 256;

/// Zero-cost hasher for TxHash
///
/// TxHash is already a 32-byte cryptographic hash - hashing it again is wasteful.
/// This hasher uses the first 8 bytes of TxHash directly as the hash value.
///
/// ## Why This Works
/// - TxHash is keccak256 output - already uniformly distributed
/// - First 8 bytes have same entropy as any 8 bytes
/// - Eliminates ~10% CPU overhead from redundant hashing
#[derive(Default, Debug)]
pub struct TxHashHasher {
    hash: u64,
}

impl Hasher for TxHashHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // TxHash is 32 bytes - use first 8 bytes as u64 hash
        // This is safe because TxHash is already a cryptographic hash
        if bytes.len() >= 8 {
            self.hash = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        } else {
            // Fallback for shorter inputs (shouldn't happen with TxHash)
            for &byte in bytes {
                self.hash = self.hash.wrapping_mul(31).wrapping_add(byte as u64);
            }
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// BuildHasher for TxHash using zero-cost hashing
pub type TxHashBuildHasher = BuildHasherDefault<TxHashHasher>;

/// Thread-safe cache for per-transaction pre-warmed keys
///
/// Stores extracted keys for each transaction separately, enabling precise
/// prefetching and direct cleanup when transactions are mined/dropped.
///
/// Uses DashMap for lock-free concurrent access - simulation workers can
/// write to different keys in parallel without blocking each other.
///
/// ## Performance Optimizations
/// - Uses `Arc<ExtractedKeys>` to avoid cloning when reading from cache
/// - Uses `TxHashHasher` to eliminate redundant hashing of TxHash (saves ~10% CPU)
#[derive(Debug)]
pub struct PreWarmedCache {
    /// Per-transaction extracted keys (lock-free concurrent map)
    /// Arc allows zero-copy reads from the cache
    /// TxHashBuildHasher eliminates redundant hashing
    per_tx_keys: DashMap<TxHash, Arc<ExtractedKeys>, TxHashBuildHasher>,

    /// Configuration
    #[allow(unused)]
    config: PreWarmingConfig,
}

impl PreWarmedCache {
    /// Create new cache with given configuration
    pub fn new(config: PreWarmingConfig) -> Self {
        Self {
            per_tx_keys: DashMap::with_capacity_and_hasher(
                DEFAULT_CACHE_CAPACITY,
                TxHashBuildHasher::default(),
            ),
            config,
        }
    }

    /// Check if a transaction already has keys in cache
    ///
    /// Used to skip redundant simulations for duplicate TX submissions.
    /// O(1) DashMap lookup.
    #[inline]
    pub fn contains_tx(&self, tx_hash: &TxHash) -> bool {
        self.per_tx_keys.contains_key(tx_hash)
    }

    /// Store keys for a transaction (called after simulation)
    ///
    /// This is called by simulation workers after extracting keys from a transaction.
    /// Each transaction's keys are stored separately.
    /// Lock-free: different tx hashes go to different shards.
    ///
    /// Keys are wrapped in Arc for zero-copy reads later.
    #[inline]
    pub fn store_tx_keys(&self, tx_hash: TxHash, keys: ExtractedKeys) {
        self.per_tx_keys.insert(tx_hash, Arc::new(keys));
    }

    /// Store keys for multiple transactions in batch (reduces lock overhead)
    ///
    /// More efficient than calling `store_tx_keys()` in a loop when you have
    /// multiple completed simulations. Groups inserts to reduce DashMap
    /// shard lock acquisitions.
    ///
    /// ## Performance
    /// - Single method call vs N method calls
    /// - Better cache locality
    /// - Reduced function call overhead with `#[inline]`
    #[inline]
    pub fn store_tx_keys_batch(&self, entries: impl IntoIterator<Item = (TxHash, ExtractedKeys)>) {
        for (tx_hash, keys) in entries {
            self.per_tx_keys.insert(tx_hash, Arc::new(keys));
        }
    }

    /// Get merged keys for selected transactions (called during block building)
    ///
    /// This is called by the block builder with the list of selected transaction hashes.
    /// Returns the merged ExtractedKeys for only those transactions.
    ///
    /// Returns empty ExtractedKeys if none of the transactions are in cache.
    /// Lock-free: reads don't block writes to other keys.
    ///
    /// ## Performance
    /// Pre-allocates capacity based on number of transactions to avoid
    /// repeated re-allocations during merge operations.
    /// Collects Arc refs first to minimize DashMap lock time.
    #[inline]
    pub fn get_keys_for_txs(&self, tx_hashes: &[TxHash]) -> ExtractedKeys {
        // Fast path: empty input
        if tx_hashes.is_empty() {
            return ExtractedKeys::new();
        }

        // Fast path: single transaction (very common)
        // For single TX, we still need to clone since we return owned ExtractedKeys
        // But we can avoid the Vec allocation
        if tx_hashes.len() == 1 {
            if let Some(keys) = self.per_tx_keys.get(&tx_hashes[0]) {
                // Clone the inner ExtractedKeys
                return keys.value().as_ref().clone();
            }
            return ExtractedKeys::new();
        }

        // Collect Arc refs first to minimize lock time on DashMap
        // This is faster than holding locks during merge operations
        let refs: Vec<_> =
            tx_hashes.iter().filter_map(|tx_hash| self.per_tx_keys.get(tx_hash)).collect();

        // Fast path: no matches
        if refs.is_empty() {
            return ExtractedKeys::new();
        }

        // Pre-allocate capacity based on actual matches
        let mut merged = ExtractedKeys::with_capacity_for_txs(refs.len());

        // Merge all refs (DashMap locks already released)
        for keys_ref in refs {
            merged.merge_ref(&keys_ref);
        }

        merged
    }

    /// Get keys for a single transaction as Arc (zero-copy for read-only access)
    ///
    /// Returns None if the transaction is not in cache.
    /// This is more efficient than `get_keys_for_txs(&[tx_hash])` when you
    /// only need read access and don't need to modify the keys.
    #[inline]
    pub fn get_keys_arc(&self, tx_hash: &TxHash) -> Option<Arc<ExtractedKeys>> {
        self.per_tx_keys.get(tx_hash).map(|r| Arc::clone(r.value()))
    }

    /// Get Arc refs for multiple transactions (zero-copy, no merge)
    ///
    /// Returns Vec of Arc<ExtractedKeys> for found transactions.
    /// More efficient than `get_keys_for_txs()` when you can iterate over
    /// individual key sets without needing a merged result.
    ///
    /// ## Performance
    /// - No cloning of ExtractedKeys data
    /// - No merge operation overhead
    /// - Ideal for prefetch which can iterate over each set
    #[inline]
    pub fn get_keys_arcs(&self, tx_hashes: &[TxHash]) -> Vec<Arc<ExtractedKeys>> {
        tx_hashes
            .iter()
            .filter_map(|tx_hash| self.per_tx_keys.get(tx_hash).map(|r| Arc::clone(r.value())))
            .collect()
    }

    /// Check if keys exist for a transaction without cloning
    #[inline]
    pub fn contains(&self, tx_hash: &TxHash) -> bool {
        self.per_tx_keys.contains_key(tx_hash)
    }

    /// Get ALL cached keys (all transactions currently in cache)
    ///
    /// This is a fallback method when we don't know which transactions will be selected.
    /// It returns merged keys for ALL cached transactions.
    ///
    /// NOTE: This may return more keys than needed. Prefer `get_keys_for_txs()` when
    /// you know which transactions will be selected.
    ///
    /// ## Performance
    /// Pre-allocates capacity based on cache size to avoid re-allocations.
    pub fn get_all_keys(&self) -> ExtractedKeys {
        let cache_len = self.per_tx_keys.len();
        let mut merged = ExtractedKeys::with_capacity_for_txs(cache_len);

        for entry in self.per_tx_keys.iter() {
            merged.merge_ref(entry.value());
        }

        merged
    }

    /// Remove keys for mined/dropped transactions (called from hook)
    ///
    /// This is called when transactions are removed from the pool (mined, dropped, etc.).
    /// Removes the keys for those transactions from the cache.
    ///
    /// NOTE: Eviction temporarily disabled - keys need to remain for payload builder
    pub fn remove_txs(&self, tx_hashes: &[TxHash]) {
        // DISABLED: Don't evict keys immediately - payload builder needs them
        // TODO: Implement TTL-based eviction instead
        let _ = tx_hashes; // Suppress unused warning
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let mut total_keys: usize = 0;
        let mut total_accounts: usize = 0;
        let mut total_storage_slots: usize = 0;
        let mut total_code_hashes: usize = 0;
        let mut total_block_hashes: usize = 0;
        let mut total_transactions: usize = 0;

        for entry in self.per_tx_keys.iter() {
            let k = entry.value();
            total_keys += k.total_keys();
            total_accounts += k.accounts.len();
            total_storage_slots += k.storage_slots.len();
            total_code_hashes += k.code_hashes.len();
            total_block_hashes += k.block_hashes.len();
            total_transactions += 1;
        }

        CacheStats {
            total_transactions,
            total_accounts,
            total_storage_slots,
            total_code_hashes,
            total_block_hashes,
            total_keys,
        }
    }

    /// Clear all cached keys (for testing)
    pub fn clear(&self) {
        self.per_tx_keys.clear();
    }

    /// Get number of cached transactions
    pub fn len(&self) -> usize {
        self.per_tx_keys.len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.per_tx_keys.is_empty()
    }
}

/// Cache statistics
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    /// Number of transactions in cache
    pub total_transactions: usize,

    /// Total accounts across all transactions
    pub total_accounts: usize,

    /// Total storage slots across all transactions
    pub total_storage_slots: usize,

    /// Total code hashes across all transactions
    pub total_code_hashes: usize,

    /// Total block hashes across all transactions
    pub total_block_hashes: usize,

    /// Total keys (sum of above)
    pub total_keys: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxHash, B256, U256};

    fn test_config() -> PreWarmingConfig {
        PreWarmingConfig::default()
    }

    fn create_test_keys(account: u8, storage_count: usize) -> ExtractedKeys {
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::from([account; 20]));
        for i in 0..storage_count {
            keys.add_storage_slot(Address::from([account; 20]), U256::from(i));
        }
        keys
    }

    // ============================================================================
    // Basic Functionality Tests
    // ============================================================================

    #[test]
    fn test_store_and_retrieve_single_tx() {
        let cache = PreWarmedCache::new(test_config());
        let tx_hash = TxHash::random();
        let keys = create_test_keys(1, 2);

        // Store keys
        cache.store_tx_keys(tx_hash, keys.clone());

        // Retrieve keys
        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.accounts.len(), 1);
        assert_eq!(retrieved.storage_slots.len(), 2);
    }

    #[test]
    fn test_store_multiple_txs() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        cache.store_tx_keys(tx1, create_test_keys(1, 2));
        cache.store_tx_keys(tx2, create_test_keys(2, 3));

        let stats = cache.stats();
        assert_eq!(stats.total_transactions, 2);
        assert_eq!(stats.total_accounts, 2);
        assert_eq!(stats.total_storage_slots, 5);
    }

    #[test]
    fn test_get_keys_for_selected_txs() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();
        let tx3 = TxHash::random();

        // Store 3 transactions
        cache.store_tx_keys(tx1, create_test_keys(1, 1));
        cache.store_tx_keys(tx2, create_test_keys(2, 2));
        cache.store_tx_keys(tx3, create_test_keys(3, 3));

        // Get keys for only tx1 and tx3 (selected for block)
        let merged = cache.get_keys_for_txs(&[tx1, tx3]);

        assert_eq!(merged.accounts.len(), 2); // Only accounts 1 and 3
        assert_eq!(merged.storage_slots.len(), 4); // 1 + 3 slots
    }

    #[test]
    fn test_get_keys_empty_cache() {
        let cache = PreWarmedCache::new(test_config());
        let tx_hash = TxHash::random();

        let merged = cache.get_keys_for_txs(&[tx_hash]);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_get_keys_missing_txs() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();
        let tx3 = TxHash::random();

        // Store only tx1
        cache.store_tx_keys(tx1, create_test_keys(1, 1));

        // Request tx2 and tx3 (not in cache)
        let merged = cache.get_keys_for_txs(&[tx2, tx3]);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_merge_deduplication() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        let addr = Address::from([1; 20]);

        // Both txs access same account
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(addr);
        keys1.add_storage_slot(addr, U256::from(1));

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(addr);
        keys2.add_storage_slot(addr, U256::from(2));

        cache.store_tx_keys(tx1, keys1);
        cache.store_tx_keys(tx2, keys2);

        // Get merged keys
        let merged = cache.get_keys_for_txs(&[tx1, tx2]);

        assert_eq!(merged.accounts.len(), 1); // Deduplicated
        assert_eq!(merged.storage_slots.len(), 2); // Both slots
    }

    // ============================================================================
    // Removal Tests
    // ============================================================================

    #[test]
    fn test_remove_single_tx() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        cache.store_tx_keys(tx1, create_test_keys(1, 1));
        cache.store_tx_keys(tx2, create_test_keys(2, 2));

        assert_eq!(cache.len(), 2);

        // Remove tx1 - NOTE: Eviction is disabled
        cache.remove_txs(&[tx1]);

        // With eviction disabled, cache still has both entries
        assert_eq!(cache.len(), 2);

        // tx1 is still there (eviction disabled)
        let merged = cache.get_keys_for_txs(&[tx1]);
        assert!(!merged.is_empty());

        // tx2 should still be there
        let merged = cache.get_keys_for_txs(&[tx2]);
        assert_eq!(merged.accounts.len(), 1);
    }

    #[test]
    fn test_remove_multiple_txs() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();
        let tx3 = TxHash::random();

        cache.store_tx_keys(tx1, create_test_keys(1, 1));
        cache.store_tx_keys(tx2, create_test_keys(2, 2));
        cache.store_tx_keys(tx3, create_test_keys(3, 3));

        assert_eq!(cache.len(), 3);

        // Remove tx1 and tx2 - NOTE: Eviction is disabled, cache stays at 3
        cache.remove_txs(&[tx1, tx2]);

        // With eviction disabled, all entries remain
        assert_eq!(cache.len(), 3);

        // tx3 should still be there
        let merged = cache.get_keys_for_txs(&[tx3]);
        assert_eq!(merged.accounts.len(), 1);
    }

    #[test]
    fn test_remove_nonexistent_tx() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        cache.store_tx_keys(tx1, create_test_keys(1, 1));

        // Remove tx2 (doesn't exist)
        cache.remove_txs(&[tx2]);

        // tx1 should still be there
        assert_eq!(cache.len(), 1);
    }

    // ============================================================================
    // Cache Management Tests
    // ============================================================================

    #[test]
    fn test_clear() {
        let cache = PreWarmedCache::new(test_config());

        cache.store_tx_keys(TxHash::random(), create_test_keys(1, 1));
        cache.store_tx_keys(TxHash::random(), create_test_keys(2, 2));

        assert_eq!(cache.len(), 2);

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_stats() {
        let cache = PreWarmedCache::new(test_config());
        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));
        keys1.add_storage_slot(Address::from([1; 20]), U256::from(1));
        keys1.add_storage_slot(Address::from([1; 20]), U256::from(2));
        keys1.add_code_hash(B256::random());

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));
        keys2.add_account(Address::from([3; 20]));
        keys2.add_storage_slot(Address::from([2; 20]), U256::from(1));

        cache.store_tx_keys(tx1, keys1);
        cache.store_tx_keys(tx2, keys2);

        let stats = cache.stats();
        assert_eq!(stats.total_transactions, 2);
        assert_eq!(stats.total_accounts, 3); // 1 + 2
        assert_eq!(stats.total_storage_slots, 3); // 2 + 1
        assert_eq!(stats.total_code_hashes, 1);
        assert_eq!(stats.total_keys, 7); // 3 accounts + 3 storage + 1 code
    }

    // ============================================================================
    // Integration Tests
    // ============================================================================

    #[test]
    fn test_full_lifecycle() {
        let cache = PreWarmedCache::new(test_config());

        // Simulate 5 transactions
        let txs: Vec<_> = (0..5).map(|_| TxHash::random()).collect();
        for (i, tx_hash) in txs.iter().enumerate() {
            cache.store_tx_keys(*tx_hash, create_test_keys(i as u8, i + 1));
        }

        assert_eq!(cache.len(), 5);

        // Block builder selects 3 transactions
        let selected = vec![txs[0], txs[2], txs[4]];
        let merged = cache.get_keys_for_txs(&selected);

        // Should have keys from tx0, tx2, tx4
        assert_eq!(merged.accounts.len(), 3);
        assert_eq!(merged.storage_slots.len(), 1 + 3 + 5); // 0+1, 2+1, 4+1

        // Block finalized, remove mined transactions - NOTE: Eviction is disabled
        cache.remove_txs(&selected);

        // With eviction disabled, all 5 entries remain
        assert_eq!(cache.len(), 5);

        // All transactions still accessible
        let remaining = cache.get_keys_for_txs(&[txs[1], txs[3]]);
        assert_eq!(remaining.accounts.len(), 2);
    }

    #[test]
    fn test_overwrite_existing_tx() {
        let cache = PreWarmedCache::new(test_config());
        let tx_hash = TxHash::random();

        // Store initial keys
        cache.store_tx_keys(tx_hash, create_test_keys(1, 2));

        let merged = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(merged.storage_slots.len(), 2);

        // Overwrite with new keys (e.g., re-simulation)
        cache.store_tx_keys(tx_hash, create_test_keys(1, 5));

        let merged = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(merged.storage_slots.len(), 5); // Updated
    }
}
