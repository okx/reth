//! Three-layer DatabaseRef for parallel transaction execution.
//!
//! Read path: curr_state cache → prev_state cache → StateProvider (QMDB/MDBX)
//!
//! This implements revm's `DatabaseRef` trait, enabling parallel executor
//! threads to read state from a concurrent cache backed by DashMap or HashMap.

use alloy_primitives::{Address, B256, U256};
use reth_storage_api::StateProvider;
use reth_storage_errors::provider::ProviderError;
use revm_state::AccountInfo;

/// Trait for concurrent state cache lookups.
///
/// Abstracted to allow different cache implementations (DashMap, HashMap, etc.)
/// without coupling the database adapter to a specific cache type.
///
/// Return semantics for `get_account`:
/// - `None` → cache miss (key not tracked)
/// - `Some(None)` → account confirmed absent (destroyed or non-existent)
/// - `Some(Some(info))` → cached account info
pub trait StateCache: Send + Sync {
    /// Look up an account in the cache.
    fn get_account(&self, address: &Address) -> Option<Option<AccountInfo>>;
    /// Look up a storage value in the cache.
    fn get_storage(&self, address: &Address, slot: &U256) -> Option<U256>;
    /// Look up bytecode by its hash.
    fn get_bytecode(&self, hash: &B256) -> Option<revm_bytecode::Bytecode>;
    /// Look up a block hash by block number.
    fn get_block_hash(&self, number: &u64) -> Option<B256>;
}

/// Three-layer DatabaseRef for parallel execution.
///
/// Each executor thread creates one of these (cheap — just references).
/// Multiple threads can read concurrently since all layers are `Sync`:
/// - Cache layers provide lock-free reads (DashMap sharding or immutable HashMap)
/// - StateProvider (QMDB) provides lock-free reads via SharedAdsWrap
pub struct ParallelTxDatabase<'a, C: StateCache> {
    /// Current block's state cache (written to by completed tasks).
    curr_state: &'a C,
    /// Previous block's state cache (read-only, for cross-block optimization).
    prev_state: Option<&'a C>,
    /// Fallback: reth StateProvider (QMDB/MDBX).
    provider: &'a (dyn StateProvider + Sync),
}

impl<'a, C: StateCache> ParallelTxDatabase<'a, C> {
    /// Create a new database with current state cache and fallback provider.
    pub fn new(curr_state: &'a C, provider: &'a (dyn StateProvider + Sync)) -> Self {
        Self { curr_state, prev_state: None, provider }
    }

    /// Create a new database with both current and previous state caches.
    pub fn with_prev_state(
        curr_state: &'a C,
        prev_state: &'a C,
        provider: &'a (dyn StateProvider + Sync),
    ) -> Self {
        Self { curr_state, prev_state: Some(prev_state), provider }
    }
}

impl<C: StateCache> core::fmt::Debug for ParallelTxDatabase<'_, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParallelTxDatabase")
            .field("has_prev_state", &self.prev_state.is_some())
            .finish()
    }
}

impl<C: StateCache> revm::DatabaseRef for ParallelTxDatabase<'_, C> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Layer 1: current block cache
        if let Some(info) = self.curr_state.get_account(&address) {
            return Ok(info);
        }

        // Layer 2: previous block cache
        if let Some(prev) = self.prev_state {
            if let Some(info) = prev.get_account(&address) {
                return Ok(info);
            }
        }

        // Layer 3: StateProvider (QMDB/MDBX)
        Ok(self.provider.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        // Layer 1
        if let Some(code) = self.curr_state.get_bytecode(&code_hash) {
            return Ok(code);
        }

        // Layer 2
        if let Some(prev) = self.prev_state {
            if let Some(code) = prev.get_bytecode(&code_hash) {
                return Ok(code);
            }
        }

        // Layer 3
        Ok(self.provider.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Layer 1
        if let Some(value) = self.curr_state.get_storage(&address, &index) {
            return Ok(value);
        }

        // Layer 2
        if let Some(prev) = self.prev_state {
            if let Some(value) = prev.get_storage(&address, &index) {
                return Ok(value);
            }
        }

        // Layer 3
        Ok(self.provider.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        // Layer 1
        if let Some(hash) = self.curr_state.get_block_hash(&number) {
            return Ok(hash);
        }

        // Layer 2
        if let Some(prev) = self.prev_state {
            if let Some(hash) = prev.get_block_hash(&number) {
                return Ok(hash);
            }
        }

        // Layer 3
        Ok(reth_storage_api::BlockHashReader::block_hash(self.provider, number)?
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::DatabaseRef;
    use std::collections::HashMap;

    /// Simple in-memory StateCache implementation for testing.
    struct MockCache {
        accounts: HashMap<Address, Option<AccountInfo>>,
        storage: HashMap<(Address, U256), U256>,
        bytecodes: HashMap<B256, revm_bytecode::Bytecode>,
        block_hashes: HashMap<u64, B256>,
    }

    impl MockCache {
        fn new() -> Self {
            Self {
                accounts: HashMap::new(),
                storage: HashMap::new(),
                bytecodes: HashMap::new(),
                block_hashes: HashMap::new(),
            }
        }

        fn set_account(&mut self, addr: Address, info: Option<AccountInfo>) {
            self.accounts.insert(addr, info);
        }

        fn set_storage(&mut self, addr: Address, slot: U256, value: U256) {
            self.storage.insert((addr, slot), value);
        }

        fn set_bytecode(&mut self, hash: B256, code: revm_bytecode::Bytecode) {
            self.bytecodes.insert(hash, code);
        }

        fn set_block_hash(&mut self, number: u64, hash: B256) {
            self.block_hashes.insert(number, hash);
        }
    }

    impl StateCache for MockCache {
        fn get_account(&self, address: &Address) -> Option<Option<AccountInfo>> {
            self.accounts.get(address).cloned()
        }

        fn get_storage(&self, address: &Address, slot: &U256) -> Option<U256> {
            self.storage.get(&(*address, *slot)).copied()
        }

        fn get_bytecode(&self, hash: &B256) -> Option<revm_bytecode::Bytecode> {
            self.bytecodes.get(hash).cloned()
        }

        fn get_block_hash(&self, number: &u64) -> Option<B256> {
            self.block_hashes.get(number).copied()
        }
    }

    /// NoopProvider returns None/default for all queries, simulating an empty database.
    fn noop_provider() -> reth_storage_api::noop::NoopProvider {
        reth_storage_api::noop::NoopProvider::mainnet()
    }

    #[test]
    fn test_basic_ref_from_curr_state() {
        let mut curr = MockCache::new();
        let addr = Address::with_last_byte(0x01);
        let info = AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() };
        curr.set_account(addr, Some(info.clone()));

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        let result = db.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(1000));
        assert_eq!(result.nonce, 5);
    }

    #[test]
    fn test_basic_ref_from_prev_state() {
        let curr = MockCache::new();
        let mut prev = MockCache::new();
        let addr = Address::with_last_byte(0x02);
        let info = AccountInfo { balance: U256::from(2000), nonce: 10, ..Default::default() };
        prev.set_account(addr, Some(info.clone()));

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        let result = db.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(2000));
        assert_eq!(result.nonce, 10);
    }

    #[test]
    fn test_storage_ref_from_curr_state() {
        let mut curr = MockCache::new();
        let addr = Address::with_last_byte(0x03);
        let slot = U256::from(7);
        curr.set_storage(addr, slot, U256::from(999));

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        let result = db.storage_ref(addr, slot).unwrap();
        assert_eq!(result, U256::from(999));
    }

    #[test]
    fn test_storage_ref_from_prev_state() {
        let curr = MockCache::new();
        let mut prev = MockCache::new();
        let addr = Address::with_last_byte(0x04);
        let slot = U256::from(3);
        prev.set_storage(addr, slot, U256::from(42));

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        let result = db.storage_ref(addr, slot).unwrap();
        assert_eq!(result, U256::from(42));
    }

    #[test]
    fn test_curr_state_overrides_prev() {
        let mut curr = MockCache::new();
        let mut prev = MockCache::new();
        let addr = Address::with_last_byte(0x05);

        // prev has old balance
        prev.set_account(
            addr,
            Some(AccountInfo { balance: U256::from(100), nonce: 1, ..Default::default() }),
        );
        // curr has updated balance
        curr.set_account(
            addr,
            Some(AccountInfo { balance: U256::from(500), nonce: 3, ..Default::default() }),
        );

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        let result = db.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(500));
        assert_eq!(result.nonce, 3);

        // Also test storage override
        let slot = U256::from(1);
        prev.set_storage(addr, slot, U256::from(10));
        curr.set_storage(addr, slot, U256::from(20));

        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);
        assert_eq!(db.storage_ref(addr, slot).unwrap(), U256::from(20));
    }

    #[test]
    fn test_cache_miss_falls_through_to_provider() {
        let curr = MockCache::new();
        let prev = MockCache::new();
        let addr = Address::with_last_byte(0x06);

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        // NoopProvider returns None for accounts → basic_ref returns None
        assert!(db.basic_ref(addr).unwrap().is_none());

        // NoopProvider returns None for storage → defaults to U256::ZERO
        assert_eq!(db.storage_ref(addr, U256::from(1)).unwrap(), U256::ZERO);

        // NoopProvider returns None for block hashes → defaults to B256::ZERO
        assert_eq!(db.block_hash_ref(42).unwrap(), B256::ZERO);
    }

    #[test]
    fn test_without_prev_state() {
        let mut curr = MockCache::new();
        let addr = Address::with_last_byte(0x07);
        curr.set_account(
            addr,
            Some(AccountInfo { balance: U256::from(300), nonce: 2, ..Default::default() }),
        );

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        // curr_state hit works
        let result = db.basic_ref(addr).unwrap().unwrap();
        assert_eq!(result.balance, U256::from(300));

        // cache miss falls through directly to provider (no prev_state)
        let unknown = Address::with_last_byte(0xFF);
        assert!(db.basic_ref(unknown).unwrap().is_none());
    }

    #[test]
    fn test_code_by_hash_from_curr() {
        let mut curr = MockCache::new();
        let hash = B256::with_last_byte(0xCC);
        let code =
            revm_bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00]));
        curr.set_bytecode(hash, code.clone());

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        let result = db.code_by_hash_ref(hash).unwrap();
        assert_eq!(result.bytes(), code.bytes());
    }

    #[test]
    fn test_code_by_hash_from_prev() {
        let curr = MockCache::new();
        let mut prev = MockCache::new();
        let hash = B256::with_last_byte(0xDD);
        let code =
            revm_bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x01]));
        prev.set_bytecode(hash, code.clone());

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        let result = db.code_by_hash_ref(hash).unwrap();
        assert_eq!(result.bytes(), code.bytes());
    }

    #[test]
    fn test_block_hash_from_curr() {
        let mut curr = MockCache::new();
        curr.set_block_hash(42, B256::with_last_byte(0xAB));

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        assert_eq!(db.block_hash_ref(42).unwrap(), B256::with_last_byte(0xAB));
    }

    #[test]
    fn test_block_hash_from_prev() {
        let curr = MockCache::new();
        let mut prev = MockCache::new();
        prev.set_block_hash(99, B256::with_last_byte(0xEF));

        let provider = noop_provider();
        let db = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);

        assert_eq!(db.block_hash_ref(99).unwrap(), B256::with_last_byte(0xEF));
    }

    #[test]
    fn test_absent_account_in_cache() {
        let mut curr = MockCache::new();
        let addr = Address::with_last_byte(0x08);
        // Explicitly mark account as absent (Some(None))
        curr.set_account(addr, None);

        let provider = noop_provider();
        let db = ParallelTxDatabase::new(&curr, &provider);

        // Should return None (absent) without falling through to provider
        assert!(db.basic_ref(addr).unwrap().is_none());
    }

    #[test]
    fn test_debug_impl() {
        let curr = MockCache::new();
        let provider = noop_provider();

        let db_no_prev = ParallelTxDatabase::new(&curr, &provider);
        let debug_str = format!("{:?}", db_no_prev);
        assert!(debug_str.contains("has_prev_state: false"));

        let prev = MockCache::new();
        let db_with_prev = ParallelTxDatabase::with_prev_state(&curr, &prev, &provider);
        let debug_str = format!("{:?}", db_with_prev);
        assert!(debug_str.contains("has_prev_state: true"));
    }
}
