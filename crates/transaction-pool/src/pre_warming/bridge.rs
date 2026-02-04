//! Prefetch utilities for pre-warming
//!
//! This module provides the PREFETCH logic that runs BEFORE execution:
//! 1. Simulation discovers KEYS (which accounts/storage to access)
//! 2. This module PREFETCHES VALUES from MDBX
//! 3. Populates CachedReads with prefetched data
//! 4. Execution reads from CachedReads (high cache hits!)
//!
//! ## Two Functions Available
//!
//! | Function | Use When | Performance |
//! |----------|----------|-------------|
//! | `prefetch_and_populate` | Have `&dyn StateProvider` (e.g., `StateProviderBox`) | Sequential |
//! | `prefetch_parallel` | Have `S: StateProvider + Sync` (e.g., via `SnapshotState`) | Parallel |

use crate::pre_warming::ExtractedKeys;
use reth_provider::StateProvider;
use reth_revm::cached::{CachedReads, CachedAccount};
use alloy_primitives::{Address, B256, U256, map::HashMap};
use std::sync::Mutex;

/// Prefetch and populate CachedReads from discovered keys (SEQUENTIAL)
///
/// Fetches values from MDBX for all keys in ExtractedKeys and populates CachedReads.
/// This runs BEFORE execution so that execution sees a warm cache.
///
/// # Note
/// Uses sequential I/O because StateProviderBox is not Sync.
/// For parallel I/O, use `prefetch_parallel` with a Sync state provider.
pub fn prefetch_and_populate(
    cached_reads: &mut CachedReads,
    keys: &ExtractedKeys,
    state_provider: &dyn StateProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Prefetch accounts
    for &address in &keys.accounts {
        if cached_reads.accounts.contains_key(&address) {
            continue;
        }

        if let Ok(account) = state_provider.basic_account(&address) {
            let info = account.map(|acc| revm::state::AccountInfo {
                balance: acc.balance,
                nonce: acc.nonce,
                code_hash: acc.bytecode_hash.unwrap_or_default(),
                code: None,
                account_id: None,
            });

            cached_reads.accounts.insert(address, CachedAccount {
                info,
                storage: HashMap::default(),
            });
        }
    }

    // Step 2: Prefetch storage slots
    for &(address, slot) in &keys.storage_slots {
        let account = cached_reads.accounts.entry(address).or_insert_with(|| {
            let info = state_provider.basic_account(&address).ok().flatten().map(|acc| {
                revm::state::AccountInfo {
                    balance: acc.balance,
                    nonce: acc.nonce,
                    code_hash: acc.bytecode_hash.unwrap_or_default(),
                    code: None,
                    account_id: None,
                }
            });
            CachedAccount {
                info,
                storage: HashMap::default(),
            }
        });

        if account.info.is_some() {
            let slot_b256 = B256::from(slot);
            if let Ok(Some(value)) = state_provider.storage(address, slot_b256) {
                account.storage.insert(slot, value);
            }
        }
    }

    // Step 3: Prefetch bytecode
    for &code_hash in &keys.code_hashes {
        if code_hash.is_zero() || cached_reads.contracts.contains_key(&code_hash) {
            continue;
        }

        if let Ok(Some(bytecode_bytes)) = state_provider.bytecode_by_hash(&code_hash) {
            let bytecode = revm::bytecode::Bytecode::new_raw(bytecode_bytes.original_bytes().clone());
            cached_reads.contracts.insert(code_hash, bytecode);
        }
    }

    Ok(())
}

/// Prefetch and populate CachedReads using PARALLEL threads (std::thread::scope)
///
/// Uses scoped threads for parallel I/O. Requires a Sync state provider.
///
/// # Arguments
/// * `cached_reads` - The cache to populate
/// * `keys` - Keys discovered by simulation
/// * `state_provider` - State provider for MDBX queries (must be Sync)
/// * `num_threads` - Number of parallel threads
///
/// # When to Use
/// Use this when you have a Sync state provider (e.g., SnapshotState).
/// For StateProviderBox (not Sync), use `prefetch_and_populate` instead.
pub fn prefetch_parallel<S>(
    cached_reads: &mut CachedReads,
    keys: &ExtractedKeys,
    state_provider: &S,
    num_threads: usize,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: StateProvider + Sync,
{
    let num_threads = num_threads.max(1);

    // Collect keys to fetch
    let accounts: Vec<Address> = keys.accounts.iter().copied().collect();
    let storage_slots: Vec<(Address, U256)> = keys.storage_slots.iter().copied().collect();
    let code_hashes: Vec<B256> = keys.code_hashes.iter().copied().collect();

    // Use Mutex to collect results from threads
    let account_results: Mutex<Vec<(Address, CachedAccount)>> = Mutex::new(Vec::new());
    let storage_results: Mutex<Vec<(Address, U256, U256)>> = Mutex::new(Vec::new());
    let bytecode_results: Mutex<Vec<(B256, revm::bytecode::Bytecode)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        // Partition accounts across threads
        let chunk_size = (accounts.len() / num_threads).max(1);
        for chunk in accounts.chunks(chunk_size) {
            let account_results = &account_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &address in chunk {
                    if let Ok(account) = state_provider.basic_account(&address) {
                        let info = account.map(|acc| revm::state::AccountInfo {
                            balance: acc.balance,
                            nonce: acc.nonce,
                            code_hash: acc.bytecode_hash.unwrap_or_default(),
                            code: None,
                            account_id: None,
                        });
                        local_results.push((address, CachedAccount {
                            info,
                            storage: HashMap::default(),
                        }));
                    }
                }
                account_results.lock().unwrap().extend(local_results);
            });
        }

        // Partition storage slots across threads
        let chunk_size = (storage_slots.len() / num_threads).max(1);
        for chunk in storage_slots.chunks(chunk_size) {
            let storage_results = &storage_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &(address, slot) in chunk {
                    let slot_b256 = B256::from(slot);
                    if let Ok(Some(value)) = state_provider.storage(address, slot_b256) {
                        local_results.push((address, slot, value));
                    }
                }
                storage_results.lock().unwrap().extend(local_results);
            });
        }

        // Partition bytecode across threads
        let chunk_size = (code_hashes.len() / num_threads).max(1);
        for chunk in code_hashes.chunks(chunk_size) {
            let bytecode_results = &bytecode_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &code_hash in chunk {
                    if code_hash.is_zero() {
                        continue;
                    }
                    if let Ok(Some(bytecode_bytes)) = state_provider.bytecode_by_hash(&code_hash) {
                        let bytecode = revm::bytecode::Bytecode::new_raw(
                            bytecode_bytes.original_bytes().clone()
                        );
                        local_results.push((code_hash, bytecode));
                    }
                }
                bytecode_results.lock().unwrap().extend(local_results);
            });
        }
    });

    // Merge results into cached_reads
    for (address, account) in account_results.into_inner().unwrap() {
        cached_reads.accounts.entry(address).or_insert(account);
    }

    for (address, slot, value) in storage_results.into_inner().unwrap() {
        let account = cached_reads.accounts.entry(address).or_insert_with(|| {
            CachedAccount {
                info: None,
                storage: HashMap::default(),
            }
        });
        account.storage.insert(slot, value);
    }

    for (code_hash, bytecode) in bytecode_results.into_inner().unwrap() {
        cached_reads.contracts.entry(code_hash).or_insert(bytecode);
    }

    Ok(())
}

/// Prefetch and populate CachedReads using SnapshotState (PARALLEL)
///
/// This is the recommended function for parallel prefetch as it works with
/// Send-only providers wrapped in SnapshotState (which provides Sync).
///
/// # Arguments
/// * `cached_reads` - The cache to populate
/// * `keys` - Keys discovered by simulation
/// * `snapshot` - SnapshotState wrapping a Send-only StateProvider
/// * `num_threads` - Number of parallel threads (default: 4)
///
/// # Example
/// ```ignore
/// let snapshot = SnapshotState::new(state_provider_box);
/// prefetch_with_snapshot(&mut cached_reads, &keys, &snapshot, 4)?;
/// ```
pub fn prefetch_with_snapshot(
    cached_reads: &mut CachedReads,
    keys: &ExtractedKeys,
    snapshot: &crate::pre_warming::SnapshotState,
    num_threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let num_threads = num_threads.max(1);

    // Collect keys to fetch
    let accounts: Vec<Address> = keys.accounts.iter().copied().collect();
    let storage_slots: Vec<(Address, U256)> = keys.storage_slots.iter().copied().collect();
    let code_hashes: Vec<B256> = keys.code_hashes.iter().copied().collect();

    // Use Mutex to collect results from threads
    let account_results: Mutex<Vec<(Address, CachedAccount)>> = Mutex::new(Vec::new());
    let storage_results: Mutex<Vec<(Address, U256, U256)>> = Mutex::new(Vec::new());
    let bytecode_results: Mutex<Vec<(B256, revm::bytecode::Bytecode)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        // Partition accounts across threads
        let chunk_size = (accounts.len() / num_threads).max(1);
        for chunk in accounts.chunks(chunk_size) {
            let account_results = &account_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &address in chunk {
                    if let Ok(info) = snapshot.basic_account(address) {
                        local_results.push((address, CachedAccount {
                            info,
                            storage: HashMap::default(),
                        }));
                    }
                }
                account_results.lock().unwrap().extend(local_results);
            });
        }

        // Partition storage slots across threads
        let chunk_size = (storage_slots.len() / num_threads).max(1);
        for chunk in storage_slots.chunks(chunk_size) {
            let storage_results = &storage_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &(address, slot) in chunk {
                    if let Ok(value) = snapshot.storage(address, slot) {
                        local_results.push((address, slot, value));
                    }
                }
                storage_results.lock().unwrap().extend(local_results);
            });
        }

        // Partition bytecode across threads
        let chunk_size = (code_hashes.len() / num_threads).max(1);
        for chunk in code_hashes.chunks(chunk_size) {
            let bytecode_results = &bytecode_results;
            s.spawn(move || {
                let mut local_results = Vec::new();
                for &code_hash in chunk {
                    if code_hash.is_zero() {
                        continue;
                    }
                    if let Ok(bytecode) = snapshot.code_by_hash(code_hash) {
                        local_results.push((code_hash, bytecode));
                    }
                }
                bytecode_results.lock().unwrap().extend(local_results);
            });
        }
    });

    // Merge results into cached_reads
    for (address, account) in account_results.into_inner().unwrap() {
        cached_reads.accounts.entry(address).or_insert(account);
    }

    for (address, slot, value) in storage_results.into_inner().unwrap() {
        let account = cached_reads.accounts.entry(address).or_insert_with(|| {
            CachedAccount {
                info: None,
                storage: HashMap::default(),
            }
        });
        account.storage.insert(slot, value);
    }

    for (code_hash, bytecode) in bytecode_results.into_inner().unwrap() {
        cached_reads.contracts.entry(code_hash).or_insert(bytecode);
    }

    Ok(())
}

/// Alternative name for backward compatibility
pub fn populate_cached_reads_from_keys(
    cached_reads: &mut CachedReads,
    keys: &ExtractedKeys,
    state_provider: &dyn StateProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    prefetch_and_populate(cached_reads, keys, state_provider)
}

/// Helper to get cache statistics after population
pub fn get_cache_stats(cached_reads: &CachedReads) -> CacheStats {
    let total_storage_slots: usize = cached_reads.accounts
        .values()
        .map(|acc| acc.storage.len())
        .sum();

    CacheStats {
        accounts_count: cached_reads.accounts.len(),
        storage_slots_count: total_storage_slots,
        contracts_count: cached_reads.contracts.len(),
        block_hashes_count: cached_reads.block_hashes.len(),
    }
}

/// Statistics about the cache contents
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of accounts in the cache
    pub accounts_count: usize,
    /// Number of storage slots in the cache
    pub storage_slots_count: usize,
    /// Number of contracts in the cache
    pub contracts_count: usize,
    /// Number of block hashes in the cache
    pub block_hashes_count: usize,
}

impl CacheStats {
    /// Total number of keys in the cache
    pub fn total_keys(&self) -> usize {
        self.accounts_count + self.storage_slots_count +
        self.contracts_count + self.block_hashes_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use revm::bytecode::Bytecode;

    // ========================================================================
    // CacheStats Tests
    // ========================================================================

    #[test]
    fn test_cache_stats_total() {
        let stats = CacheStats {
            accounts_count: 10,
            storage_slots_count: 50,
            contracts_count: 5,
            block_hashes_count: 2,
        };
        assert_eq!(stats.total_keys(), 67);
    }

    #[test]
    fn test_cache_stats_empty() {
        let stats = CacheStats {
            accounts_count: 0,
            storage_slots_count: 0,
            contracts_count: 0,
            block_hashes_count: 0,
        };
        assert_eq!(stats.total_keys(), 0);
    }

    #[test]
    fn test_cache_stats_large_numbers() {
        let stats = CacheStats {
            accounts_count: 100_000,
            storage_slots_count: 500_000,
            contracts_count: 10_000,
            block_hashes_count: 1_000,
        };
        assert_eq!(stats.total_keys(), 611_000);
    }

    // ========================================================================
    // get_cache_stats Tests
    // ========================================================================

    #[test]
    fn test_get_cache_stats_empty() {
        let cached_reads = CachedReads::default();
        let stats = get_cache_stats(&cached_reads);

        assert_eq!(stats.accounts_count, 0);
        assert_eq!(stats.storage_slots_count, 0);
        assert_eq!(stats.contracts_count, 0);
        assert_eq!(stats.block_hashes_count, 0);
    }

    #[test]
    fn test_get_cache_stats_populated() {
        let mut cached_reads = CachedReads::default();

        // Add accounts with storage
        let addr1 = Address::random();
        let mut storage1 = HashMap::default();
        storage1.insert(U256::from(1), U256::from(100));
        storage1.insert(U256::from(2), U256::from(200));
        cached_reads.accounts.insert(addr1, CachedAccount {
            info: None,
            storage: storage1,
        });

        let addr2 = Address::random();
        let mut storage2 = HashMap::default();
        storage2.insert(U256::from(3), U256::from(300));
        cached_reads.accounts.insert(addr2, CachedAccount {
            info: None,
            storage: storage2,
        });

        // Add contracts
        cached_reads.contracts.insert(B256::random(), Bytecode::new_raw(vec![].into()));
        cached_reads.contracts.insert(B256::random(), Bytecode::new_raw(vec![].into()));

        // Add block hashes
        cached_reads.block_hashes.insert(1, B256::random());

        let stats = get_cache_stats(&cached_reads);

        assert_eq!(stats.accounts_count, 2);
        assert_eq!(stats.storage_slots_count, 3); // 2 + 1
        assert_eq!(stats.contracts_count, 2);
        assert_eq!(stats.block_hashes_count, 1);
        assert_eq!(stats.total_keys(), 8);
    }

    #[test]
    fn test_get_cache_stats_accounts_only() {
        let mut cached_reads = CachedReads::default();

        for _ in 0..5 {
            cached_reads.accounts.insert(Address::random(), CachedAccount {
                info: None,
                storage: HashMap::default(),
            });
        }

        let stats = get_cache_stats(&cached_reads);
        assert_eq!(stats.accounts_count, 5);
        assert_eq!(stats.storage_slots_count, 0);
        assert_eq!(stats.contracts_count, 0);
    }

    #[test]
    fn test_get_cache_stats_contracts_only() {
        let mut cached_reads = CachedReads::default();

        for _ in 0..3 {
            cached_reads.contracts.insert(B256::random(), Bytecode::new_raw(vec![0x00].into()));
        }

        let stats = get_cache_stats(&cached_reads);
        assert_eq!(stats.accounts_count, 0);
        assert_eq!(stats.contracts_count, 3);
    }

    #[test]
    fn test_get_cache_stats_storage_calculation() {
        let mut cached_reads = CachedReads::default();

        // Account 1: 3 storage slots
        let addr1 = Address::random();
        let mut storage1 = HashMap::default();
        storage1.insert(U256::from(1), U256::from(100));
        storage1.insert(U256::from(2), U256::from(200));
        storage1.insert(U256::from(3), U256::from(300));
        cached_reads.accounts.insert(addr1, CachedAccount {
            info: None,
            storage: storage1,
        });

        // Account 2: 2 storage slots
        let addr2 = Address::random();
        let mut storage2 = HashMap::default();
        storage2.insert(U256::from(10), U256::from(1000));
        storage2.insert(U256::from(20), U256::from(2000));
        cached_reads.accounts.insert(addr2, CachedAccount {
            info: None,
            storage: storage2,
        });

        // Account 3: 0 storage slots
        let addr3 = Address::random();
        cached_reads.accounts.insert(addr3, CachedAccount {
            info: None,
            storage: HashMap::default(),
        });

        let stats = get_cache_stats(&cached_reads);
        assert_eq!(stats.accounts_count, 3);
        assert_eq!(stats.storage_slots_count, 5); // 3 + 2 + 0
    }

    // ========================================================================
    // ExtractedKeys Integration Tests
    // ========================================================================

    #[test]
    fn test_extracted_keys_empty() {
        let keys = ExtractedKeys::new();
        assert!(keys.accounts.is_empty());
        assert!(keys.storage_slots.is_empty());
        assert!(keys.code_hashes.is_empty());
    }

    #[test]
    fn test_extracted_keys_add_accounts() {
        let mut keys = ExtractedKeys::new();
        let addr1 = Address::random();
        let addr2 = Address::random();

        keys.add_account(addr1);
        keys.add_account(addr2);
        keys.add_account(addr1); // Duplicate

        assert_eq!(keys.accounts.len(), 2); // Deduped
        assert!(keys.accounts.contains(&addr1));
        assert!(keys.accounts.contains(&addr2));
    }

    #[test]
    fn test_extracted_keys_add_storage_slots() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        keys.add_storage_slot(addr, slot1);
        keys.add_storage_slot(addr, slot2);
        keys.add_storage_slot(addr, slot1); // Duplicate

        assert_eq!(keys.storage_slots.len(), 2); // Deduped
    }

    #[test]
    fn test_extracted_keys_add_code_hashes() {
        let mut keys = ExtractedKeys::new();
        let hash1 = B256::random();
        let hash2 = B256::random();

        keys.add_code_hash(hash1);
        keys.add_code_hash(hash2);
        keys.add_code_hash(hash1); // Duplicate

        assert_eq!(keys.code_hashes.len(), 2); // Deduped
    }

    #[test]
    fn test_extracted_keys_mixed() {
        let mut keys = ExtractedKeys::new();

        // Add various keys
        for _ in 0..10 {
            keys.add_account(Address::random());
        }

        let addr = Address::random();
        for i in 0..5u64 {
            keys.add_storage_slot(addr, U256::from(i));
        }

        for _ in 0..3 {
            keys.add_code_hash(B256::random());
        }

        assert_eq!(keys.accounts.len(), 10);
        assert_eq!(keys.storage_slots.len(), 5);
        assert_eq!(keys.code_hashes.len(), 3);
    }

    // ========================================================================
    // CachedAccount Tests
    // ========================================================================

    #[test]
    fn test_cached_account_with_info() {
        let info = revm::state::AccountInfo {
            balance: U256::from(1000),
            nonce: 5,
            code_hash: B256::ZERO,
            code: None,
            account_id: None,
        };

        let cached = CachedAccount {
            info: Some(info),
            storage: HashMap::default(),
        };

        assert!(cached.info.is_some());
        assert_eq!(cached.info.as_ref().unwrap().balance, U256::from(1000));
        assert_eq!(cached.info.as_ref().unwrap().nonce, 5);
    }

    #[test]
    fn test_cached_account_with_storage() {
        let mut storage = HashMap::default();
        storage.insert(U256::from(1), U256::from(100));
        storage.insert(U256::from(2), U256::from(200));

        let cached = CachedAccount {
            info: None,
            storage,
        };

        assert!(cached.info.is_none());
        assert_eq!(cached.storage.len(), 2);
        assert_eq!(cached.storage.get(&U256::from(1)), Some(&U256::from(100)));
    }

    // ========================================================================
    // CachedReads Tests
    // ========================================================================

    #[test]
    fn test_cached_reads_default() {
        let cached = CachedReads::default();
        assert!(cached.accounts.is_empty());
        assert!(cached.contracts.is_empty());
        assert!(cached.block_hashes.is_empty());
    }

    #[test]
    fn test_cached_reads_insert_account() {
        let mut cached = CachedReads::default();
        let addr = Address::random();

        cached.accounts.insert(addr, CachedAccount {
            info: Some(revm::state::AccountInfo {
                balance: U256::from(500),
                nonce: 1,
                code_hash: B256::ZERO,
                code: None,
                account_id: None,
            }),
            storage: HashMap::default(),
        });

        assert!(cached.accounts.contains_key(&addr));
        assert_eq!(cached.accounts.len(), 1);
    }

    #[test]
    fn test_cached_reads_insert_contract() {
        let mut cached = CachedReads::default();
        let code_hash = B256::random();
        let bytecode = Bytecode::new_raw(vec![0x60, 0x00].into());

        cached.contracts.insert(code_hash, bytecode);

        assert!(cached.contracts.contains_key(&code_hash));
        assert_eq!(cached.contracts.len(), 1);
    }

    #[test]
    fn test_cached_reads_insert_block_hash() {
        let mut cached = CachedReads::default();
        let block_number = 12345u64;
        let block_hash = B256::random();

        cached.block_hashes.insert(block_number, block_hash);

        assert_eq!(cached.block_hashes.get(&block_number), Some(&block_hash));
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_zero_code_hash_handling() {
        let mut keys = ExtractedKeys::new();
        keys.add_code_hash(B256::ZERO);

        // B256::ZERO should be added to the set
        assert!(keys.code_hashes.contains(&B256::ZERO));

        // But prefetch should skip it (tested in integration)
    }

    #[test]
    fn test_large_storage_slot_values() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Test with max U256 slot
        let max_slot = U256::MAX;
        keys.add_storage_slot(addr, max_slot);

        assert!(keys.storage_slots.contains(&(addr, max_slot)));
    }

    #[test]
    fn test_many_accounts() {
        let mut keys = ExtractedKeys::new();

        // Add 1000 unique accounts
        for _ in 0..1000 {
            keys.add_account(Address::random());
        }

        assert_eq!(keys.accounts.len(), 1000);
    }

    #[test]
    fn test_many_storage_slots_per_account() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Add 100 storage slots for same account
        for i in 0..100u64 {
            keys.add_storage_slot(addr, U256::from(i));
        }

        assert_eq!(keys.storage_slots.len(), 100);
    }

    #[test]
    fn test_cache_stats_clone() {
        let stats = CacheStats {
            accounts_count: 10,
            storage_slots_count: 20,
            contracts_count: 5,
            block_hashes_count: 2,
        };

        let cloned = stats.clone();
        assert_eq!(stats.accounts_count, cloned.accounts_count);
        assert_eq!(stats.storage_slots_count, cloned.storage_slots_count);
        assert_eq!(stats.contracts_count, cloned.contracts_count);
        assert_eq!(stats.block_hashes_count, cloned.block_hashes_count);
    }

    #[test]
    fn test_cache_stats_debug() {
        let stats = CacheStats {
            accounts_count: 1,
            storage_slots_count: 2,
            contracts_count: 3,
            block_hashes_count: 4,
        };

        // Should implement Debug
        let debug_str = format!("{:?}", stats);
        assert!(debug_str.contains("accounts_count"));
        assert!(debug_str.contains("storage_slots_count"));
    }
}

