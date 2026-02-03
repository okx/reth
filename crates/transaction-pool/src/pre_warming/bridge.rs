//! Prefetch utilities for pre-warming
//!
//! This module provides the PREFETCH logic that runs BEFORE execution:
//! 1. Simulation discovers KEYS (which accounts/storage to access)
//! 2. This module PREFETCHES VALUES from MDBX (can be parallel)
//! 3. Populates CachedReads with prefetched data
//! 4. Execution reads from CachedReads (high cache hits!)

use crate::pre_warming::ExtractedKeys;
use reth_provider::StateProvider;
use reth_revm::cached::{CachedReads, CachedAccount};
use alloy_primitives::{Address, B256, U256, map::HashMap};
use std::sync::Mutex;

/// Prefetch and populate CachedReads from discovered keys (SEQUENTIAL)
///
/// Use this for small key sets or when simplicity is preferred.
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
/// Uses scoped threads for parallel I/O without requiring Sync on StateProvider.
/// Each thread gets its own reference to the state provider.
///
/// # Arguments
/// * `cached_reads` - The cache to populate (protected by Mutex internally)
/// * `keys` - Keys discovered by simulation
/// * `state_provider` - State provider for MDBX queries (must be Send + Sync)
/// * `num_threads` - Number of parallel threads (default: 4)
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
}

