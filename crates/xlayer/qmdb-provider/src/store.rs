//! Typed wrapper over QMDB's low-level ADS API.
//!
//! Provides account/storage/bytecode read/write methods and state root retrieval.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Address, B256, U256};
use parking_lot::{Mutex, RwLock};
use qmdb::{
    config::Config as QmdbConfig,
    def::{IN_BLOCK_IDX_BITS, OP_CREATE, OP_WRITE},
    seqads::task::{SingleCsTask, TaskBuilder},
    tasks::TasksManager,
    AdsCore, AdsWrap, ADS,
};
use reth_primitives_traits::{Account, Bytecode};
use revm_database::BundleState;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    path::Path,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};
use xlayer_salt::account::{
    account_plain_key, decode_account, decode_storage_value, encode_account, encode_storage_value,
    storage_plain_key,
};

/// Maximum entry size for QMDB read buffer.
const MAX_ENTRY_SIZE: usize = 64 * 1024;

/// Chunk size for pre-populating QMDB.
const PRE_POP_CHUNK_SIZE: usize = 20_000;

/// Prefix byte for bytecode keys to distinguish from account/storage keys.
const BYTECODE_PREFIX: u8 = 0xFF;

/// Compute SHA-256 hash of a key (used for QMDB key_hash parameter).
#[inline]
fn sha256_key(key: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.finalize().into()
}

/// Build a bytecode key from a code hash: prefix byte + code_hash (33 bytes).
#[inline]
fn bytecode_key(code_hash: &B256) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(BYTECODE_PREFIX);
    key.extend_from_slice(code_hash.as_slice());
    key
}

/// Typed wrapper over QMDB providing account/storage/bytecode operations.
pub struct QmdbStore {
    ads: Mutex<AdsWrap<SingleCsTask>>,
    next_height: AtomicI64,
}

impl std::fmt::Debug for QmdbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbStore")
            .field("next_height", &self.next_height.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl QmdbStore {
    /// Create a new `QmdbStore` at the given path.
    ///
    /// Initializes the QMDB directory and ADS instance.
    pub fn new(path: &Path) -> Self {
        let config = QmdbConfig {
            dir: path.to_str().expect("path must be valid UTF-8").to_string(),
            ..QmdbConfig::default()
        };
        AdsCore::init_dir(&config);
        let ads = AdsWrap::<SingleCsTask>::new(&config);
        Self { ads: Mutex::new(ads), next_height: AtomicI64::new(1) }
    }

    /// Read an account from QMDB.
    pub fn read_account(&self, address: &Address) -> Option<Account> {
        let key = account_plain_key(address);
        let value = self.read_raw(&key)?;
        decode_account(&value)
    }

    /// Read a storage slot from QMDB.
    pub fn read_storage(&self, address: &Address, slot: &B256) -> Option<U256> {
        let key = storage_plain_key(address, slot);
        let value = self.read_raw(&key)?;
        decode_storage_value(&value)
    }

    /// Read bytecode by its code hash from QMDB.
    pub fn read_bytecode(&self, code_hash: &B256) -> Option<Bytecode> {
        if *code_hash == KECCAK_EMPTY {
            return None;
        }
        let key = bytecode_key(code_hash);
        let value = self.read_raw(&key)?;
        Some(Bytecode::new_raw(value.into()))
    }

    /// Commit a `BundleState` to QMDB as a new block.
    ///
    /// Converts the bundle into QMDB operations, submits the block, and flushes.
    /// Uses `OP_CREATE` for new accounts/storage and `OP_WRITE` for existing ones.
    pub fn commit_bundle(&self, bundle: &BundleState) {
        let height = self.next_height.fetch_add(1, Ordering::SeqCst);
        let task = Self::bundle_to_task_auto_op(bundle);

        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));

        let mut ads = self.ads.lock();
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
        ads.flush();
    }

    /// Commit a `BundleState` without flushing (for pipeline mode).
    ///
    /// Call [`flush`] separately after submitting all blocks.
    pub fn submit_bundle(&self, bundle: &BundleState) {
        let height = self.next_height.fetch_add(1, Ordering::SeqCst);
        let task = Self::bundle_to_task_auto_op(bundle);

        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));

        let mut ads = self.ads.lock();
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
    }

    /// Flush pending blocks to disk.
    pub fn flush(&self) {
        self.ads.lock().flush();
    }

    /// Get the QMDB state root.
    ///
    /// First tries `get_root_hash_of_height` (populated after flush). If that returns
    /// zeros, falls back to hashing all per-shard root hashes from the metadb.
    pub fn state_root(&self) -> B256 {
        let ads = self.ads.lock();
        let height = self.next_height.load(Ordering::SeqCst) - 1;
        let shared = ads.get_shared();
        let hash = shared.get_root_hash_of_height(height);

        if hash != [0u8; 32] {
            return B256::from(hash);
        }

        // Fall back: hash all per-shard root hashes together
        let metadb = ads.get_metadb();
        let mdb = metadb.read();
        let mut hasher = Sha256::new();
        for shard_id in 0..qmdb::def::SHARD_COUNT {
            hasher.update(mdb.get_root_hash(shard_id));
        }
        let combined: [u8; 32] = hasher.finalize().into();
        B256::from(combined)
    }

    /// Current QMDB height (next height to be used).
    pub fn next_height(&self) -> i64 {
        self.next_height.load(Ordering::SeqCst)
    }

    /// Pre-populate QMDB with state from a `BundleState`.
    ///
    /// Used to set up initial state before benchmark runs.
    /// Returns the number of chunks (blocks) used for pre-population.
    pub fn pre_populate(&self, bundle: &BundleState) -> i64 {
        let accounts: Vec<_> = bundle.state().iter().collect();
        let num_chunks = (accounts.len() + PRE_POP_CHUNK_SIZE - 1) / PRE_POP_CHUNK_SIZE;

        let mut ads = self.ads.lock();

        for (chunk_idx, chunk) in accounts.chunks(PRE_POP_CHUNK_SIZE).enumerate() {
            let height = (chunk_idx + 1) as i64;
            let mut builder = TaskBuilder::new();

            for (address, bundle_account) in chunk {
                let address = Address::from(**address);
                if let Some(info) = &bundle_account.info {
                    let code_hash =
                        if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
                    let account = Account {
                        nonce: info.nonce,
                        balance: info.balance,
                        bytecode_hash: code_hash,
                    };
                    builder.add_op(
                        OP_CREATE,
                        &account_plain_key(&address),
                        &encode_account(&account),
                    );

                    // Store bytecode if present
                    if let Some(code) = &info.code {
                        if info.code_hash != KECCAK_EMPTY {
                            let key = bytecode_key(&info.code_hash);
                            builder.add_op(OP_CREATE, &key, code.bytes_slice());
                        }
                    }
                }
                for (slot, slot_info) in &bundle_account.storage {
                    let slot_b256 = B256::from(*slot);
                    builder.add_op(
                        OP_CREATE,
                        &storage_plain_key(&address, &slot_b256),
                        &encode_storage_value(&slot_info.present_value),
                    );
                }
            }

            let task = builder.build();
            let task_id: i64 = height << IN_BLOCK_IDX_BITS;
            let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
            ads.start_block(height, tasks_manager);
            let shared = ads.get_shared();
            shared.insert_extra_data(height, String::new());
            shared.add_task(task_id);
        }

        ads.flush();

        let num = num_chunks.max(1) as i64;
        self.next_height.store(num + 1, Ordering::SeqCst);
        num
    }

    /// Read raw bytes from QMDB for a given key.
    ///
    /// Uses a thread-local buffer to avoid 64KB heap allocation per read.
    fn read_raw(&self, key: &[u8]) -> Option<Vec<u8>> {
        thread_local! {
            static READ_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; MAX_ENTRY_SIZE]);
        }

        let key_hash = sha256_key(key);
        let height = self.next_height.load(Ordering::SeqCst) - 1;

        READ_BUF.with(|buf_cell| {
            let mut buf = buf_cell.borrow_mut();

            let ads = self.ads.lock();
            let shared = ads.get_shared();
            let (size, found) = shared.read_entry(height, &key_hash, key, &mut buf);

            if !found {
                return None;
            }

            // If the buffer was too small, retry with a larger one
            if size > buf.len() {
                buf.resize(size, 0);
                let (size2, found2) = shared.read_entry(height, &key_hash, key, &mut buf);
                if !found2 || size2 != size {
                    return None;
                }
            }

            // EntryBz format (from entry.rs):
            //   [0..4]: u32 LE encoding (value_len << 8 | key_len)
            //   [4]:    deactivated SN count
            //   [5..5+key_len]: key bytes
            //   [5+key_len..5+key_len+value_len]: value bytes
            //   followed by: next_key_hash (32), version (8), serial_number (8), ...
            if size < 5 {
                return None;
            }
            let first32 = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            let key_len = (first32 & 0xff) as usize;
            let value_len = (first32 >> 8) as usize;

            if value_len == 0 {
                return None;
            }

            let value_start = 5 + key_len;
            let value_end = value_start + value_len;
            if value_end > size {
                return None;
            }

            Some(buf[value_start..value_end].to_vec())
        })
    }

    /// Convert a `BundleState` into a QMDB task, automatically choosing
    /// `OP_CREATE` for new entries and `OP_WRITE` for existing ones.
    fn bundle_to_task_auto_op(bundle: &BundleState) -> SingleCsTask {
        let mut builder = TaskBuilder::new();

        for (address, bundle_account) in bundle.state() {
            let address = Address::from(*address);

            // Account is new if it had no original_info (didn't exist before this block)
            let account_is_new = bundle_account.original_info.is_none();
            let account_op = if account_is_new { OP_CREATE } else { OP_WRITE };

            if let Some(info) = &bundle_account.info {
                let code_hash =
                    if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
                let account =
                    Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
                builder.add_op(account_op, &account_plain_key(&address), &encode_account(&account));

                // Bytecode is always CREATE since it's content-addressed and immutable
                if let Some(code) = &info.code {
                    if info.code_hash != KECCAK_EMPTY {
                        let key = bytecode_key(&info.code_hash);
                        builder.add_op(OP_CREATE, &key, code.bytes_slice());
                    }
                }
            }

            for (slot, slot_info) in &bundle_account.storage {
                let slot_b256 = B256::from(*slot);
                // Storage slot is new if its previous/original value was zero (non-existent)
                let storage_op = if slot_info.previous_or_original_value.is_zero() {
                    OP_CREATE
                } else {
                    OP_WRITE
                };
                builder.add_op(
                    storage_op,
                    &storage_plain_key(&address, &slot_b256),
                    &encode_storage_value(&slot_info.present_value),
                );
            }
        }

        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::map::HashMap;
    use revm_database::{
        states::StorageSlot, AccountStatus, BundleAccount, StorageWithOriginalValues,
    };
    use revm_state::AccountInfo;

    fn make_test_bundle(accounts: Vec<(Address, u64, U256, Vec<(B256, U256)>)>) -> BundleState {
        let mut state = HashMap::default();
        for (addr, nonce, balance, slots) in accounts {
            let info = AccountInfo {
                nonce,
                balance,
                code_hash: KECCAK_EMPTY,
                code: None,
                account_id: None,
            };
            let mut storage = StorageWithOriginalValues::default();
            for (slot, value) in slots {
                storage.insert(slot.into(), StorageSlot::new_changed(U256::ZERO, value));
            }
            state.insert(
                addr,
                BundleAccount {
                    info: Some(info),
                    original_info: None,
                    storage,
                    status: AccountStatus::Changed,
                },
            );
        }
        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    #[test]
    fn test_account_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = QmdbStore::new(dir.path());

        let addr = Address::from([0x01; 20]);
        let bundle = make_test_bundle(vec![(addr, 42, U256::from(1_000_000u64), vec![])]);

        store.pre_populate(&bundle);

        let account = store.read_account(&addr).expect("account should exist");
        assert_eq!(account.nonce, 42);
        assert_eq!(account.balance, U256::from(1_000_000u64));
        assert_eq!(account.bytecode_hash, None);
    }

    #[test]
    fn test_storage_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = QmdbStore::new(dir.path());

        let addr = Address::from([0x02; 20]);
        let slot = B256::from([0xaa; 32]);
        let value = U256::from(999u64);
        let bundle = make_test_bundle(vec![(addr, 0, U256::ZERO, vec![(slot, value)])]);

        store.pre_populate(&bundle);

        let read_value = store.read_storage(&addr, &slot).expect("storage should exist");
        assert_eq!(read_value, value);
    }

    #[test]
    fn test_nonexistent_account() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = QmdbStore::new(dir.path());

        // Pre-populate with one account so height > 0
        let addr = Address::from([0x01; 20]);
        let bundle = make_test_bundle(vec![(addr, 1, U256::from(1u64), vec![])]);
        store.pre_populate(&bundle);

        let missing = Address::from([0xFF; 20]);
        assert!(store.read_account(&missing).is_none());
    }

    #[test]
    fn test_commit_bundle_updates_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = QmdbStore::new(dir.path());

        // Pre-populate
        let addr = Address::from([0x01; 20]);
        let bundle = make_test_bundle(vec![(addr, 1, U256::from(100u64), vec![])]);
        store.pre_populate(&bundle);

        // Commit update
        let updated_bundle = make_test_bundle(vec![(addr, 2, U256::from(200u64), vec![])]);
        store.commit_bundle(&updated_bundle);

        let account = store.read_account(&addr).expect("account should exist");
        assert_eq!(account.nonce, 2);
        assert_eq!(account.balance, U256::from(200u64));
    }

    #[test]
    fn test_state_root_changes_after_commit() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = QmdbStore::new(dir.path());

        let addr = Address::from([0x01; 20]);
        let bundle = make_test_bundle(vec![(addr, 1, U256::from(100u64), vec![])]);
        store.pre_populate(&bundle);

        let root1 = store.state_root();

        let bundle2 = make_test_bundle(vec![(addr, 2, U256::from(200u64), vec![])]);
        store.commit_bundle(&bundle2);

        let root2 = store.state_root();
        assert_ne!(root1, root2, "state root should change after commit");
    }
}
