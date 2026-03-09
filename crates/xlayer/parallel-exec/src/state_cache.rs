//! Parallel-safe state cache for concurrent EVM execution.
//!
//! Provides a three-layer read path:
//! 1. [`ParallelStateCache`] (DashMap-based, current block's hot data)
//! 2. reth `StateProvider` fallback (cross-block cache + QMDB/MDBX)
//! 3. Persistent storage accessed via the `StateProvider`

use alloy_primitives::{Address, B256, U256};
use dashmap::DashMap;
use revm_state::AccountInfo;

/// Thread-safe state cache for parallel EVM execution.
///
/// Serves as Layer 1 in a two-layer read path:
/// 1. `ParallelStateCache` (DashMap-based, hot intra-block data)
/// 2. reth `StateProvider` fallback (cross-block cache + QMDB/MDBX)
///
/// DashMap provides lock-free concurrent reads and fine-grained per-shard
/// locking on writes, avoiding a single global lock bottleneck when many
/// EVM threads access state simultaneously.
#[derive(Debug)]
pub struct ParallelStateCache {
    /// Cached accounts: Address -> Option<AccountInfo> (None = confirmed non-existent)
    accounts: DashMap<Address, Option<AccountInfo>>,
    /// Cached storage: (Address, U256) -> Option<U256> (None = confirmed non-existent)
    storage: DashMap<(Address, U256), Option<U256>>,
    /// Cached bytecodes: code_hash -> revm Bytecode
    bytecodes: DashMap<B256, revm_bytecode::Bytecode>,
    /// Cached block hashes: block_number -> hash
    block_hashes: DashMap<u64, B256>,
}

/// Summary statistics for the cache contents.
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    /// Number of accounts in the cache.
    pub accounts_cached: usize,
    /// Number of storage slots in the cache.
    pub storage_cached: usize,
    /// Number of bytecodes in the cache.
    pub bytecodes_cached: usize,
}

impl ParallelStateCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            bytecodes: DashMap::new(),
            block_hashes: DashMap::new(),
        }
    }

    /// Insert or update an account in the cache.
    pub fn insert_account(&self, address: Address, info: Option<AccountInfo>) {
        self.accounts.insert(address, info);
    }

    /// Get a cached account.
    ///
    /// Returns `None` on cache miss (the account may or may not exist on-chain).
    /// Returns `Some(None)` when the cache confirms the account does not exist.
    /// Returns `Some(Some(info))` when the account is cached.
    pub fn get_account(&self, address: &Address) -> Option<Option<AccountInfo>> {
        self.accounts.get(address).map(|v| v.value().clone())
    }

    /// Insert or update a storage value.
    pub fn insert_storage(&self, address: Address, slot: U256, value: Option<U256>) {
        self.storage.insert((address, slot), value);
    }

    /// Get a cached storage value.
    ///
    /// Returns `None` on cache miss. `Some(None)` means the slot is confirmed empty.
    pub fn get_storage(&self, address: &Address, slot: &U256) -> Option<Option<U256>> {
        self.storage.get(&(*address, *slot)).map(|v| *v.value())
    }

    /// Insert bytecode by its code hash.
    pub fn insert_bytecode(&self, hash: B256, code: revm_bytecode::Bytecode) {
        self.bytecodes.insert(hash, code);
    }

    /// Get cached bytecode by its code hash.
    pub fn get_bytecode(&self, hash: &B256) -> Option<revm_bytecode::Bytecode> {
        self.bytecodes.get(hash).map(|v| v.value().clone())
    }

    /// Insert a block hash.
    pub fn insert_block_hash(&self, number: u64, hash: B256) {
        self.block_hashes.insert(number, hash);
    }

    /// Get a cached block hash.
    pub fn get_block_hash(&self, number: &u64) -> Option<B256> {
        self.block_hashes.get(number).map(|v| *v.value())
    }

    /// Return current cache statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            accounts_cached: self.accounts.len(),
            storage_cached: self.storage.len(),
            bytecodes_cached: self.bytecodes.len(),
        }
    }

    /// Create a new cache, optionally seeded from a previous block's cache.
    ///
    /// For the MVP this returns a fresh cache. The fallback `StateProvider`
    /// (e.g. `MemoryOverlayStateProvider`) already provides cross-block caching,
    /// so selective carry-over is a future optimization.
    pub fn new_with_prev(_prev: &ParallelStateCache) -> Self {
        Self::new()
    }

    /// Clear all cached data.
    pub fn clear(&self) {
        self.accounts.clear();
        self.storage.clear();
        self.bytecodes.clear();
        self.block_hashes.clear();
    }
}

impl Default for ParallelStateCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CachedStateProvider: DatabaseRef implementation
// ---------------------------------------------------------------------------

use reth_storage_api::StateProvider;
use reth_storage_errors::provider::ProviderError;
use revm::DatabaseRef;

/// Wraps a [`ParallelStateCache`] and a fallback [`StateProvider`] to implement
/// revm's [`DatabaseRef`].
///
/// Each parallel EVM thread receives a `CachedStateProvider` that first checks
/// the shared DashMap cache (lock-free reads) and falls back to the underlying
/// state provider on cache miss, populating the cache for subsequent lookups.
pub struct CachedStateProvider<'a> {
    /// Shared parallel state cache (DashMap-based).
    cache: &'a ParallelStateCache,
    /// Fallback: reth StateProvider (MemoryOverlayStateProvider -> QMDB/MDBX)
    fallback: &'a dyn StateProvider,
}

impl core::fmt::Debug for CachedStateProvider<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CachedStateProvider").finish_non_exhaustive()
    }
}

impl<'a> CachedStateProvider<'a> {
    /// Create a new provider with the given cache and fallback.
    pub fn new(cache: &'a ParallelStateCache, fallback: &'a dyn StateProvider) -> Self {
        Self { cache, fallback }
    }
}

impl DatabaseRef for CachedStateProvider<'_> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(cached) = self.cache.get_account(&address) {
            return Ok(cached);
        }
        // Fallback to StateProvider; Account -> AccountInfo via From impl
        let info = self.fallback.basic_account(&address)?.map(Into::into);
        self.cache.insert_account(address, info.clone());
        Ok(info)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        if let Some(cached) = self.cache.get_bytecode(&code_hash) {
            return Ok(cached);
        }
        // reth_primitives_traits::Bytecode wraps revm Bytecode; extract inner via .0
        let bytecode = self.fallback.bytecode_by_hash(&code_hash)?.unwrap_or_default().0;
        self.cache.insert_bytecode(code_hash, bytecode.clone());
        Ok(bytecode)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(cached) = self.cache.get_storage(&address, &index) {
            return Ok(cached.unwrap_or(U256::ZERO));
        }
        let value =
            self.fallback.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default();
        self.cache.insert_storage(address, index, Some(value));
        Ok(value)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(cached) = self.cache.get_block_hash(&number) {
            return Ok(cached);
        }
        let hash = reth_storage_api::BlockHashReader::block_hash(self.fallback, number)?
            .unwrap_or_default();
        self.cache.insert_block_hash(number, hash);
        Ok(hash)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_cache_account_insert_and_get() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(1);
        let info = AccountInfo { balance: U256::from(100), nonce: 42, ..Default::default() };

        cache.insert_account(addr, Some(info.clone()));
        let result = cache.get_account(&addr);
        assert!(result.is_some());
        let inner = result.unwrap().unwrap();
        assert_eq!(inner.balance, U256::from(100));
        assert_eq!(inner.nonce, 42);
    }

    #[test]
    fn test_cache_account_none() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(2);

        // Insert None to mark account as confirmed non-existent
        cache.insert_account(addr, None);
        let result = cache.get_account(&addr);
        // Some(None) = cached, and the cached value is "does not exist"
        assert_eq!(result, Some(None));
    }

    #[test]
    fn test_cache_storage_insert_and_get() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(3);
        let slot = U256::from(7);
        let value = U256::from(999);

        cache.insert_storage(addr, slot, Some(value));
        let result = cache.get_storage(&addr, &slot);
        assert_eq!(result, Some(Some(value)));
    }

    #[test]
    fn test_cache_miss() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(99);

        // Nothing inserted -> cache miss (None), distinct from "confirmed absent" (Some(None))
        assert!(cache.get_account(&addr).is_none());
        assert!(cache.get_storage(&addr, &U256::ZERO).is_none());
        assert!(cache.get_bytecode(&B256::ZERO).is_none());
        assert!(cache.get_block_hash(&0).is_none());
    }

    #[test]
    fn test_cache_stats() {
        let cache = ParallelStateCache::new();
        cache.insert_account(Address::with_last_byte(1), Some(AccountInfo::default()));
        cache.insert_account(Address::with_last_byte(2), None);
        cache.insert_storage(Address::with_last_byte(1), U256::from(0), Some(U256::ZERO));
        cache.insert_bytecode(B256::with_last_byte(1), revm_bytecode::Bytecode::default());

        let stats = cache.stats();
        assert_eq!(stats.accounts_cached, 2);
        assert_eq!(stats.storage_cached, 1);
        assert_eq!(stats.bytecodes_cached, 1);
    }

    #[test]
    fn test_cache_clear() {
        let cache = ParallelStateCache::new();
        cache.insert_account(Address::with_last_byte(1), Some(AccountInfo::default()));
        cache.insert_storage(Address::with_last_byte(1), U256::from(0), Some(U256::ZERO));
        cache.insert_bytecode(B256::with_last_byte(1), revm_bytecode::Bytecode::default());
        cache.insert_block_hash(42, B256::with_last_byte(42));

        cache.clear();

        assert!(cache.get_account(&Address::with_last_byte(1)).is_none());
        assert!(cache.get_storage(&Address::with_last_byte(1), &U256::from(0)).is_none());
        assert!(cache.get_bytecode(&B256::with_last_byte(1)).is_none());
        assert!(cache.get_block_hash(&42).is_none());
        assert_eq!(cache.stats().accounts_cached, 0);
    }

    #[test]
    fn test_concurrent_access() {
        let cache = Arc::new(ParallelStateCache::new());
        let handles: Vec<_> = (0..8u16)
            .map(|i| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for j in 0..100u16 {
                        // Use u16 arithmetic to avoid u8 overflow, then truncate for Address
                        let byte = ((i * 100 + j) % 256) as u8;
                        let addr = Address::with_last_byte(byte);
                        cache.insert_account(addr, Some(AccountInfo::default()));
                        cache.get_account(&addr);

                        let slot = U256::from(j as u64);
                        cache.insert_storage(addr, slot, Some(U256::from(j as u64)));
                        cache.get_storage(&addr, &slot);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Threads may overlap on the same address byte, but the important
        // property is that concurrent DashMap access causes no panics.
        assert!(cache.stats().accounts_cached > 0);
    }
}
