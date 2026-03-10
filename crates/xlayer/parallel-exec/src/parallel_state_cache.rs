//! DashMap-based concurrent state cache for true parallel execution.
//!
//! Unlike [`FrameStateOverlay`](crate::state_cache::FrameStateOverlay) which requires sequential
//! writes between frames, [`ParallelStateCache`] supports concurrent reads and writes during
//! execution. Multiple executor threads can simultaneously read cached state and apply state diffs
//! as their tasks complete.
//!
//! Two-layer lookup: curr_state (current block) → prev_state (previous block).

use alloy_primitives::{Address, B256, U256};
use dashmap::DashMap;
use revm_state::AccountInfo;
use std::sync::Arc;

/// Concurrent state cache backed by DashMap.
///
/// Provides lock-free concurrent reads and fine-grained concurrent writes
/// (DashMap uses internal sharding to minimize contention).
///
/// State lookup priority:
/// 1. This cache (current block's accumulated state)
/// 2. Previous block's cache (if provided, for cross-block optimization)
/// 3. StateProvider fallback (QMDB/MDBX) — handled by ParallelTxDatabase
#[derive(Debug, Clone)]
pub struct ParallelStateCache {
    /// Account info cache: Address -> Option<AccountInfo>
    /// None means the account is confirmed absent/destroyed.
    accounts: Arc<DashMap<Address, Option<AccountInfo>>>,
    /// Storage cache: (Address, U256 slot) -> U256 value
    storage: Arc<DashMap<(Address, U256), U256>>,
    /// Bytecode cache: code_hash -> Bytecode
    bytecodes: Arc<DashMap<B256, revm_bytecode::Bytecode>>,
    /// Block hash cache: block_number -> hash
    block_hashes: Arc<DashMap<u64, B256>>,
}

impl Default for ParallelStateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ParallelStateCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            accounts: Arc::new(DashMap::new()),
            storage: Arc::new(DashMap::new()),
            bytecodes: Arc::new(DashMap::new()),
            block_hashes: Arc::new(DashMap::new()),
        }
    }

    // -----------------------------------------------------------------------
    // Lookups (concurrent-safe, used by executor threads)
    // -----------------------------------------------------------------------

    /// Look up an account. Returns:
    /// - `None` → cache miss (not tracked)
    /// - `Some(None)` → account confirmed absent
    /// - `Some(Some(info))` → cached account info
    pub fn get_account(&self, address: &Address) -> Option<Option<AccountInfo>> {
        self.accounts.get(address).map(|entry| entry.value().clone())
    }

    /// Look up a storage value.
    pub fn get_storage(&self, address: &Address, slot: &U256) -> Option<U256> {
        self.storage.get(&(*address, *slot)).map(|entry| *entry.value())
    }

    /// Look up bytecode by hash.
    pub fn get_bytecode(&self, hash: &B256) -> Option<revm_bytecode::Bytecode> {
        self.bytecodes.get(hash).map(|entry| entry.value().clone())
    }

    /// Look up a block hash.
    pub fn get_block_hash(&self, number: &u64) -> Option<B256> {
        self.block_hashes.get(number).map(|entry| *entry.value())
    }

    // -----------------------------------------------------------------------
    // Writes (concurrent-safe, called by executor threads after tx completion)
    // -----------------------------------------------------------------------

    /// Insert or update an account.
    pub fn insert_account(&self, address: Address, info: Option<AccountInfo>) {
        self.accounts.insert(address, info);
    }

    /// Insert or update a storage slot.
    pub fn insert_storage(&self, address: Address, slot: U256, value: U256) {
        self.storage.insert((address, slot), value);
    }

    /// Insert bytecode.
    pub fn insert_bytecode(&self, hash: B256, code: revm_bytecode::Bytecode) {
        self.bytecodes.insert(hash, code);
    }

    /// Insert a block hash.
    pub fn insert_block_hash(&self, number: u64, hash: B256) {
        self.block_hashes.insert(number, hash);
    }

    // -----------------------------------------------------------------------
    // Batch apply (from revm execution results)
    // -----------------------------------------------------------------------

    /// Apply EVM state diff from a completed transaction to the cache.
    ///
    /// This is called by executor threads after each transaction completes.
    /// DashMap's internal sharding ensures minimal contention even when
    /// multiple threads call this simultaneously.
    pub fn apply_evm_state(&self, state: &revm::state::EvmState) {
        for (address, account) in state {
            let info = AccountInfo {
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                code: account.info.code.clone(),
                account_id: None,
            };
            self.accounts.insert(*address, Some(info));

            // Cache any new bytecode
            if let Some(ref code) = account.info.code {
                if !code.is_empty() {
                    self.bytecodes.insert(account.info.code_hash, code.clone());
                }
            }

            for (slot, value) in &account.storage {
                self.storage.insert((*address, *slot), value.present_value);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Stats and utilities
    // -----------------------------------------------------------------------

    /// Number of cached accounts.
    pub fn accounts_len(&self) -> usize {
        self.accounts.len()
    }

    /// Number of cached storage slots.
    pub fn storage_len(&self) -> usize {
        self.storage.len()
    }

    /// Number of cached bytecodes.
    pub fn bytecodes_len(&self) -> usize {
        self.bytecodes.len()
    }

    /// Clear all cached data (for reuse across blocks).
    pub fn clear(&self) {
        self.accounts.clear();
        self.storage.clear();
        self.bytecodes.clear();
        self.block_hashes.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let cache = ParallelStateCache::new();
        assert_eq!(cache.accounts_len(), 0);
        assert_eq!(cache.storage_len(), 0);
        assert_eq!(cache.bytecodes_len(), 0);
    }

    #[test]
    fn test_insert_and_get_account() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);
        let info = AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() };

        cache.insert_account(addr, Some(info.clone()));

        let got = cache.get_account(&addr).unwrap().unwrap();
        assert_eq!(got.balance, U256::from(1000));
        assert_eq!(got.nonce, 5);
    }

    #[test]
    fn test_insert_and_get_storage() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);
        let slot = U256::from(7);
        let value = U256::from(999);

        cache.insert_storage(addr, slot, value);

        assert_eq!(cache.get_storage(&addr, &slot), Some(value));
    }

    #[test]
    fn test_insert_and_get_bytecode() {
        let cache = ParallelStateCache::new();
        let hash = B256::with_last_byte(0xCC);
        let code =
            revm_bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00]));

        cache.insert_bytecode(hash, code.clone());

        let got = cache.get_bytecode(&hash).unwrap();
        assert_eq!(got.bytes(), code.bytes());
    }

    #[test]
    fn test_get_miss() {
        let cache = ParallelStateCache::new();
        assert!(cache.get_account(&Address::with_last_byte(0x01)).is_none());
        assert!(cache.get_storage(&Address::with_last_byte(0x01), &U256::from(1)).is_none());
        assert!(cache.get_bytecode(&B256::with_last_byte(0x01)).is_none());
        assert!(cache.get_block_hash(&42).is_none());
    }

    #[test]
    fn test_account_absent() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x01);

        // Insert None to mark account as absent/destroyed
        cache.insert_account(addr, None);

        // Should return Some(None) — confirmed absent, not a cache miss
        let result = cache.get_account(&addr);
        assert!(result.is_some(), "expected Some for tracked address");
        assert!(result.unwrap().is_none(), "expected None (absent account)");
    }

    #[test]
    fn test_apply_evm_state() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);

        let mut state = revm::state::EvmState::default();
        let mut storage = revm_state::EvmStorage::default();
        storage.insert(
            U256::from(7),
            revm_state::EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(999),
                ..Default::default()
            },
        );

        let account = revm_state::Account {
            info: AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() },
            original_info: Box::new(AccountInfo::default()),
            status: revm_state::AccountStatus::Touched,
            storage,
            transaction_id: 0,
        };
        state.insert(addr, account);

        cache.apply_evm_state(&state);

        // Account should be cached
        let info = cache.get_account(&addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(1000));
        assert_eq!(info.nonce, 5);

        // Storage should be cached
        assert_eq!(cache.get_storage(&addr, &U256::from(7)), Some(U256::from(999)));

        // Non-existent entries should be None
        assert!(cache.get_account(&Address::with_last_byte(0x99)).is_none());
        assert!(cache.get_storage(&addr, &U256::from(8)).is_none());
    }

    #[test]
    fn test_apply_evm_state_overwrites() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0xA0);

        // First apply
        let mut state1 = revm::state::EvmState::default();
        state1.insert(
            addr,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(100), nonce: 1, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        cache.apply_evm_state(&state1);

        // Second apply with different values
        let mut state2 = revm::state::EvmState::default();
        state2.insert(
            addr,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(50), nonce: 2, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        cache.apply_evm_state(&state2);

        // Latest value should win
        let info = cache.get_account(&addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(50));
        assert_eq!(info.nonce, 2);
    }

    #[test]
    fn test_clear() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);

        cache.insert_account(addr, Some(AccountInfo::default()));
        cache.insert_storage(addr, U256::from(1), U256::from(42));
        cache.insert_bytecode(
            B256::with_last_byte(0xCC),
            revm_bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00])),
        );
        cache.insert_block_hash(1, B256::with_last_byte(0xAB));

        assert!(cache.accounts_len() > 0);
        assert!(cache.storage_len() > 0);
        assert!(cache.bytecodes_len() > 0);

        cache.clear();

        assert_eq!(cache.accounts_len(), 0);
        assert_eq!(cache.storage_len(), 0);
        assert_eq!(cache.bytecodes_len(), 0);
        assert!(cache.get_block_hash(&1).is_none());
    }

    #[test]
    fn test_concurrent_reads_and_writes() {
        use std::sync::Arc;
        let cache = Arc::new(ParallelStateCache::new());
        let num_threads = 8;
        let ops_per_thread = 100;

        let mut handles = Vec::new();

        // Spawn writer threads (even indices)
        for t in 0..num_threads {
            let cache = Arc::clone(&cache);
            handles.push(std::thread::spawn(move || {
                for i in 0..ops_per_thread {
                    let byte = ((t * ops_per_thread + i) % 256) as u8;
                    let addr = Address::with_last_byte(byte);
                    cache.insert_account(
                        addr,
                        Some(AccountInfo {
                            balance: U256::from(t * 1000 + i),
                            nonce: (t * ops_per_thread + i) as u64,
                            ..Default::default()
                        }),
                    );
                    cache.insert_storage(addr, U256::from(i), U256::from(t * i));
                }
            }));
        }

        // Spawn reader threads
        for _ in 0..num_threads {
            let cache = Arc::clone(&cache);
            handles.push(std::thread::spawn(move || {
                for i in 0..ops_per_thread {
                    let byte = (i % 256) as u8;
                    let addr = Address::with_last_byte(byte);
                    // These may or may not find data depending on timing — that's fine.
                    // The important thing is no panics or deadlocks.
                    let _ = cache.get_account(&addr);
                    let _ = cache.get_storage(&addr, &U256::from(i));
                }
            }));
        }

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        // After all threads complete, verify the cache has data
        assert!(cache.accounts_len() > 0, "cache should have accounts after concurrent writes");
        assert!(cache.storage_len() > 0, "cache should have storage after concurrent writes");
    }

    #[test]
    fn test_clone_shares_data() {
        let cache = ParallelStateCache::new();
        let addr = Address::with_last_byte(0x42);

        // Insert via original
        cache.insert_account(
            addr,
            Some(AccountInfo { balance: U256::from(123), ..Default::default() }),
        );

        // Clone and verify data is visible
        let cloned = cache.clone();
        let info = cloned.get_account(&addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(123));

        // Insert via clone, verify visible in original (Arc sharing)
        let addr2 = Address::with_last_byte(0x99);
        cloned.insert_account(
            addr2,
            Some(AccountInfo { balance: U256::from(456), ..Default::default() }),
        );
        let info2 = cache.get_account(&addr2).unwrap().unwrap();
        assert_eq!(info2.balance, U256::from(456));
    }
}
