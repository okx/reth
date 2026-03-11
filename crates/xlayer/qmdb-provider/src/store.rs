//! Typed wrapper over QMDB's low-level ADS API.
//!
//! Provides account/storage read/write methods and state root retrieval.
//! Bytecodes are NOT stored in QMDB — they stay in MDBX.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Address, B256, U256};
use parking_lot::{Mutex, RwLock};
use qmdb::{
    config::Config as QmdbConfig,
    def::{IN_BLOCK_IDX_BITS, OP_CREATE, OP_WRITE},
    seqads::task::{SingleCsTask, TaskBuilder},
    tasks::TasksManager,
    AdsCore, AdsWrap, SharedAdsWrap, ADS,
};
use reth_primitives_traits::Account;
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

/// Compute SHA-256 hash of a key (used for QMDB key_hash parameter).
#[inline]
fn sha256_key(key: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.finalize().into()
}

/// Message sent to the background flusher thread.
enum FlushMsg {
    /// Bundle to submit + flush.
    Bundle(BundleState),
}

/// Typed wrapper over QMDB providing account/storage/bytecode operations.
pub struct QmdbStore {
    /// Exclusive access for write operations (start_block, flush).
    ads: Mutex<AdsWrap<SingleCsTask>>,
    /// Shared handle for concurrent reads. Updated after pre_populate/start_block
    /// to pick up properly initialized EntryCache.
    shared: parking_lot::RwLock<SharedAdsWrap>,
    next_height: AtomicI64,
    /// Number of pending commits from payload builder that on_canonical_commit should skip.
    pending_skips: AtomicI64,
    /// Last flushed root hash, cached for immediate access.
    last_flushed_root: parking_lot::RwLock<B256>,
    /// Channel to send work to the background flusher thread.
    flush_tx: std::sync::mpsc::Sender<FlushMsg>,
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
    /// Initializes the QMDB directory, ADS instance, and background flusher thread.
    /// The flusher thread handles all QMDB mutations (submit + flush) to avoid
    /// Mutex contention with the payload builder thread.
    pub fn new(path: &Path) -> Arc<Self> {
        let config = QmdbConfig {
            dir: path.to_str().expect("path must be valid UTF-8").to_string(),
            ..QmdbConfig::default()
        };
        AdsCore::init_dir(&config);
        let ads = AdsWrap::<SingleCsTask>::new(&config);
        let shared = ads.get_shared();

        let (flush_tx, flush_rx) = std::sync::mpsc::channel::<FlushMsg>();

        let store = Arc::new(Self {
            ads: Mutex::new(ads),
            shared: parking_lot::RwLock::new(shared),
            next_height: AtomicI64::new(1),
            pending_skips: AtomicI64::new(0),
            last_flushed_root: parking_lot::RwLock::new(B256::ZERO),
            flush_tx,
        });

        // Background flusher thread (like fafo's FlusherShard + Updater combined).
        // Does all QMDB mutations: bundle conversion → submit → flush → root update.
        // The payload builder thread NEVER touches the ads Mutex — zero contention.
        let flusher_store = Arc::clone(&store);
        std::thread::Builder::new()
            .name("qmdb-flusher".to_string())
            .spawn(move || {
                while let Ok(FlushMsg::Bundle(first_bundle)) = flush_rx.recv() {
                    // Collect all pending bundles (coalesce into one flush)
                    let mut bundles: Vec<BundleState> = vec![first_bundle];
                    while let Ok(FlushMsg::Bundle(b)) = flush_rx.try_recv() {
                        bundles.push(b);
                    }

                    // Submit all bundles under one lock acquisition
                    let mut ads = flusher_store.ads.lock();
                    for bundle in &bundles {
                        let height = flusher_store.next_height.fetch_add(1, Ordering::SeqCst);
                        let task = Self::bundle_to_task_auto_op(bundle);
                        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
                        let tasks_manager =
                            Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
                        ads.start_block(height, tasks_manager);
                        let shared = ads.get_shared();
                        shared.insert_extra_data(height, String::new());
                        shared.add_task(task_id);
                    }

                    // Flush all submitted bundles at once
                    ads.flush();
                    drop(ads);

                    // Update cached root after flush completes
                    let height = flusher_store.next_height.load(Ordering::SeqCst) - 1;
                    let hash = flusher_store.shared.read().get_root_hash_of_height(height);
                    let root = if hash != [0u8; 32] {
                        B256::from(hash)
                    } else {
                        let ads = flusher_store.ads.lock();
                        let metadb = ads.get_metadb();
                        let mdb = metadb.read();
                        let mut hasher = Sha256::new();
                        for shard_id in 0..qmdb::def::SHARD_COUNT {
                            hasher.update(mdb.get_root_hash(shard_id));
                        }
                        B256::from(<[u8; 32]>::from(hasher.finalize()))
                    };
                    *flusher_store.last_flushed_root.write() = root;
                }
            })
            .expect("failed to spawn qmdb-flusher thread");

        store
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

    /// Commit a `BundleState` to QMDB as a new block (synchronous).
    ///
    /// Submits and flushes directly (used by `on_canonical_commit`).
    pub fn commit_bundle(&self, bundle: &BundleState) {
        self.submit_bundle(bundle);
        self.flush();
        *self.last_flushed_root.write() = self.state_root();
    }

    /// Send a bundle to the flusher thread for async processing.
    /// Used by `on_canonical_commit` when the payload builder didn't already handle it.
    pub fn commit_bundle_async(&self, bundle: BundleState) {
        let _ = self.flush_tx.send(FlushMsg::Bundle(bundle));
    }

    /// Send state to the background flusher thread (truly non-blocking).
    ///
    /// The payload builder calls this instead of `commit_bundle`. The bundle is sent
    /// over a channel — no Mutex contention, no waiting for flush. The flusher thread
    /// does conversion + submit + flush asynchronously.
    ///
    /// Use `last_flushed_root()` to get the most recent root.
    pub fn submit_bundle_async(&self, bundle: BundleState) {
        self.pending_skips.fetch_add(1, Ordering::SeqCst);
        let _ = self.flush_tx.send(FlushMsg::Bundle(bundle));
    }

    /// Get the last flushed state root (computed by the background flush thread).
    ///
    /// This may be one block behind if the background flush hasn't completed yet.
    /// With `skip_state_root_validation`, this is acceptable — correctness comes
    /// from `on_canonical_commit` which runs the "official" QMDB write path.
    pub fn last_flushed_root(&self) -> B256 {
        *self.last_flushed_root.read()
    }

    /// Check if the next canonical commit should be skipped (already committed by payload builder).
    /// Returns true if skipped, false if the caller should proceed with commit_bundle.
    pub fn skip_if_already_committed(&self) -> bool {
        let prev = self.pending_skips.fetch_sub(1, Ordering::SeqCst);
        if prev > 0 {
            true // Skip this commit — payload builder already did it
        } else {
            // Restore the counter (was already 0 or negative)
            self.pending_skips.fetch_add(1, Ordering::SeqCst);
            false
        }
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
        let height = self.next_height.load(Ordering::SeqCst) - 1;
        let hash = self.shared.read().get_root_hash_of_height(height);

        if hash != [0u8; 32] {
            return B256::from(hash);
        }

        // Fall back: hash all per-shard root hashes together
        let ads = self.ads.lock();
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

                    // Bytecodes are NOT stored in QMDB — they stay in MDBX.
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

        // Update shared with properly initialized cache from latest start_block
        *self.shared.write() = ads.get_shared();

        drop(ads);

        let num = num_chunks.max(1) as i64;
        self.next_height.store(num + 1, Ordering::SeqCst);

        // Update cached root after pre-population flush
        *self.last_flushed_root.write() = self.state_root();

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

            let (size, found) = self.shared.read().read_entry(height, &key_hash, key, &mut buf);

            if !found {
                return None;
            }

            // If the buffer was too small, retry with a larger one
            if size > buf.len() {
                buf.resize(size, 0);
                let (size2, found2) =
                    self.shared.read().read_entry(height, &key_hash, key, &mut buf);
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

    /// Convert a `BundleState` into a QMDB task.
    ///
    /// Always uses `OP_CREATE` for safety: with async flushing, QMDB may lag
    /// behind reth's state, so a key that reth considers "existing" might not
    /// yet be in QMDB. `OP_CREATE` handles both new and existing keys correctly.
    fn bundle_to_task_auto_op(bundle: &BundleState) -> SingleCsTask {
        let mut builder = TaskBuilder::new();

        for (address, bundle_account) in bundle.state() {
            let address = Address::from(*address);

            if let Some(info) = &bundle_account.info {
                let code_hash =
                    if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
                let account =
                    Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
                builder.add_op(OP_CREATE, &account_plain_key(&address), &encode_account(&account));
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
