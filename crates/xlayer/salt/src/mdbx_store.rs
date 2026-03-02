//! MDBX-backed persistent storage for SALT.
//!
//! Uses `reth_libmdbx` directly (bypassing reth's table abstraction) to provide
//! disk-backed SALT operations for realistic I/O benchmarking.
//!
//! ## Performance-critical design decisions
//!
//! 1. **Cursor-based writes**: All writes use MDBX cursors on sorted data (BTreeMap order) instead
//!    of individual `tx.put()` calls. Cursors maintain their position in the B-tree, so sequential
//!    sorted writes are O(1) amortized per entry. `tx.put()` would search from root each time =
//!    O(log N) per entry, which is 5-10x slower at 200K+ entries.
//!
//! 2. **Cached RO transactions**: Read operations reuse a single RO transaction instead of creating
//!    a new one per call. `EphemeralSaltState` makes thousands of individual reads during
//!    `update_fin()` (linear probing per key). Per-call transaction creation adds ~1µs overhead ×
//!    ~4000 calls = ~4ms wasted. Cached txn reduces this to ~0.
//!
//! 3. **In-memory slot counts**: Bucket slot counts live entirely in memory (`HashMap`), avoiding a
//!    third MDBX table. This halves MDBX write ops and dirty pages per commit.

use reth_libmdbx::{
    CommitLatency, DatabaseFlags, Environment, EnvironmentFlags, Geometry, Mode, PageSize,
    SyncMode, Transaction, WriteFlags, RO, RW,
};
use salt::{
    constant::default_commitment,
    traits::{StateReader, TrieReader},
    types::*,
    StateUpdates, TrieUpdates,
};
use std::{
    collections::HashMap,
    ops::{Range, RangeInclusive},
    path::Path,
    sync::Mutex,
    time::Duration,
};

const DB_STATE: &str = "salt_state";
const DB_TRIE: &str = "salt_trie";

/// Errors from the MDBX-backed SALT store.
#[derive(Debug, thiserror::Error)]
pub enum MdbxSaltError {
    /// SALT logic error.
    #[error("SALT: {0}")]
    Salt(#[from] SaltError),
    /// MDBX I/O error.
    #[error("MDBX: {0}")]
    Mdbx(#[from] reth_libmdbx::Error),
}

/// Statistics from a single MDBX write batch (state or trie).
#[derive(Debug)]
pub struct WriteStats {
    /// Number of logical entries written (put/del operations).
    pub entries: usize,
    /// Logical bytes of key+value data written.
    pub logical_bytes: usize,
    /// MDBX commit latency breakdown (write syscalls, fsync, GC, etc.).
    pub commit_latency: CommitLatency,
}

impl WriteStats {
    /// Duration of `write()` syscalls during commit.
    pub fn write_duration(&self) -> Duration {
        self.commit_latency.write()
    }

    /// Duration of `fdatasync()`/`msync()` during commit.
    pub fn sync_duration(&self) -> Duration {
        self.commit_latency.sync()
    }

    /// Total commit duration.
    pub fn commit_duration(&self) -> Duration {
        self.commit_latency.whole()
    }
}

/// MDBX-backed persistent storage for SALT.
///
/// Uses two named databases within a single MDBX environment:
/// - `salt_state`: `SaltKey` (8 bytes BE) → `SaltValue` (variable, ≤94 bytes)
/// - `salt_trie`: `NodeId` (8 bytes BE) → `CommitmentBytes` (64 bytes)
///
/// Bucket slot counts are kept purely in memory to avoid writing to a third table
/// on every block. This halves the MDBX write operations and dirty pages per commit,
/// matching the I/O profile of reth's DupSort-based MPT storage.
pub struct MdbxSaltStore {
    env: Environment,
    state_dbi: u32,
    trie_dbi: u32,
    /// In-memory bucket slot counts — avoids 2×N extra MDBX ops per block.
    slot_counts: Mutex<HashMap<BucketId, u64>>,
    /// Cached RO transaction shared across `StateReader`/`TrieReader` calls.
    /// Uses `Mutex` because SALT's `StateRoot` uses rayon for parallel trie updates,
    /// which may call `TrieReader::commitment()` from multiple threads.
    /// Invalidated before each write operation so subsequent reads see committed data.
    ro_txn_cache: Mutex<Option<Transaction<RO>>>,
}

impl std::fmt::Debug for MdbxSaltStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdbxSaltStore").finish()
    }
}

// SAFETY: The MDBX environment and DBI handles are thread-safe.
// DBI handles are valid for the lifetime of the environment.
// All shared state (slot_counts, ro_txn_cache) is protected by Mutex.
unsafe impl Send for MdbxSaltStore {}
unsafe impl Sync for MdbxSaltStore {}

impl MdbxSaltStore {
    /// Opens or creates an MDBX-backed SALT store at the given path.
    ///
    /// Uses the same MDBX configuration as reth for fair benchmarking:
    /// `WriteMap` mode, optimized page size, and matched geometry settings.
    pub fn new(path: &Path) -> Result<Self, MdbxSaltError> {
        let mut builder = Environment::builder();
        builder.set_max_dbs(4);

        builder.write_map();

        // Use OS page size to match reth's MDBX configuration.
        // On macOS ARM64 this is 16KB; using 4KB would create 4× more pages
        // and 4× more dirty pages on msync, causing artificially slow commits.
        let os_page_size = page_size::get().clamp(4096, 0x10000);

        builder.set_geometry(Geometry::<std::ops::Range<usize>> {
            size: Some(0..(8 * 1024 * 1024 * 1024 * 1024)), // 8TB, same as reth
            growth_step: Some(4 * 1024 * 1024 * 1024),      // 4GB, same as reth
            shrink_threshold: Some(0),
            page_size: Some(PageSize::Set(os_page_size)),
        });
        builder.set_flags(EnvironmentFlags {
            mode: Mode::ReadWrite { sync_mode: SyncMode::Durable },
            no_rdahead: true,
            coalesce: true,
            ..Default::default()
        });
        // Match reth's rp_augment_limit to prioritize freelist lookup speed
        builder.set_rp_augment_limit(256 * 1024);

        let env = builder.open(path)?;

        let (state_dbi, trie_dbi);
        {
            let tx = env.begin_rw_txn()?;
            let state_db = tx.create_db(Some(DB_STATE), DatabaseFlags::empty())?;
            let trie_db = tx.create_db(Some(DB_TRIE), DatabaseFlags::empty())?;
            state_dbi = state_db.dbi();
            trie_dbi = trie_db.dbi();
            tx.commit()?;
        }

        Ok(Self {
            env,
            state_dbi,
            trie_dbi,
            slot_counts: Mutex::new(HashMap::new()),
            ro_txn_cache: Mutex::new(None),
        })
    }

    /// Returns a cached RO transaction, creating one if needed.
    /// The transaction is `Arc`-backed (`Clone` is cheap), so callers get their own handle
    /// while sharing the same underlying MDBX snapshot.
    /// Thread-safe: lock is held only briefly to clone/create the `Arc`-backed transaction.
    fn get_or_create_ro_txn(&self) -> Result<Transaction<RO>, MdbxSaltError> {
        let mut cache = self.ro_txn_cache.lock().unwrap();
        if let Some(ref tx) = *cache {
            return Ok(tx.clone());
        }
        let tx = self.env.begin_ro_txn()?;
        *cache = Some(tx.clone());
        Ok(tx)
    }

    /// Drops the cached RO transaction so the next read sees freshly committed data.
    fn invalidate_ro_cache(&self) {
        *self.ro_txn_cache.lock().unwrap() = None;
    }

    fn rw_txn(&self) -> Result<Transaction<RW>, MdbxSaltError> {
        Ok(self.env.begin_rw_txn()?)
    }

    /// Applies state updates and commits to disk.
    ///
    /// Bucket slot counts are updated purely in memory (zero MDBX ops).
    /// Only flat state entries are written to MDBX via cursor on sorted BTreeMap data.
    pub fn update_state(&self, updates: StateUpdates) -> Result<WriteStats, MdbxSaltError> {
        self.invalidate_ro_cache();
        let tx = self.rw_txn()?;

        let mut entries = 0usize;
        let mut bytes = 0usize;

        // Phase 1: update bucket slot counts in memory (zero MDBX I/O)
        {
            let mut counts = self.slot_counts.lock().unwrap();
            for (key, (old_value, new_value)) in &updates.data {
                if !key.is_in_meta_bucket() {
                    let delta: i64 = match (old_value.is_some(), new_value.is_some()) {
                        (false, true) => 1,
                        (true, false) => -1,
                        _ => 0,
                    };
                    if delta != 0 {
                        let count = counts.entry(key.bucket_id()).or_insert(0);
                        *count = (*count as i64 + delta).max(0) as u64;
                    }
                }
            }
        }

        // Phase 2: write state entries using cursor (keys sorted by BTreeMap)
        {
            let mut state_cursor = tx.cursor(self.state_dbi)?;
            for (key, (_old_value, new_value)) in updates.data {
                let key_bytes = key.0.to_be_bytes();
                match new_value {
                    Some(val) => {
                        let data_len = val.data_len();
                        let val_bytes = &val.data[..data_len];
                        state_cursor.put(&key_bytes, val_bytes, WriteFlags::UPSERT)?;
                        bytes += 8 + data_len;
                    }
                    None => {
                        if state_cursor.set::<Vec<u8>>(&key_bytes)?.is_some() {
                            state_cursor.del(WriteFlags::empty())?;
                        }
                        bytes += 8;
                    }
                }
                entries += 1;
            }
        }

        let latency = tx.commit()?;
        Ok(WriteStats { entries, logical_bytes: bytes, commit_latency: latency })
    }

    /// Applies trie updates and commits to disk using cursor writes.
    pub fn update_trie(&self, updates: TrieUpdates) -> Result<WriteStats, MdbxSaltError> {
        self.invalidate_ro_cache();
        let tx = self.rw_txn()?;

        let count = updates.len();
        let mut bytes = 0usize;

        let mut cursor = tx.cursor(self.trie_dbi)?;
        for (node_id, (_, new_commitment)) in updates {
            let key = node_id.to_be_bytes();
            cursor.put(&key, &new_commitment, WriteFlags::UPSERT)?;
            bytes += 8 + 64;
        }

        let latency = tx.commit()?;
        Ok(WriteStats { entries: count, logical_bytes: bytes, commit_latency: latency })
    }

    /// Returns MDBX environment statistics (page counts, depth, etc.).
    pub fn env_stat(&self) -> Result<reth_libmdbx::Stat, MdbxSaltError> {
        Ok(self.env.stat()?)
    }

    /// Forces sync to disk.
    pub fn sync(&self) -> Result<(), MdbxSaltError> {
        self.env.sync(true)?;
        Ok(())
    }
}

impl StateReader for MdbxSaltStore {
    type Error = MdbxSaltError;

    fn value(&self, key: SaltKey) -> Result<Option<SaltValue>, Self::Error> {
        let tx = self.get_or_create_ro_txn()?;
        let key_bytes = key.0.to_be_bytes();
        let result: Option<Vec<u8>> = tx.get(self.state_dbi, &key_bytes)?;

        match result {
            Some(raw) => {
                let mut data = [0u8; MAX_SALT_VALUE_BYTES];
                let len = raw.len().min(MAX_SALT_VALUE_BYTES);
                data[..len].copy_from_slice(&raw[..len]);
                Ok(Some(SaltValue { data }))
            }
            None => Ok(None),
        }
    }

    fn entries(
        &self,
        range: RangeInclusive<SaltKey>,
    ) -> Result<Vec<(SaltKey, SaltValue)>, Self::Error> {
        let tx = self.get_or_create_ro_txn()?;
        let mut cursor = tx.cursor(self.state_dbi)?;

        let start_bytes = range.start().0.to_be_bytes();
        let end_key = *range.end();
        let mut result = Vec::new();

        let iter = cursor.iter_from::<Vec<u8>, Vec<u8>>(&start_bytes).filter_map(|item| item.ok());

        for (k, v) in iter {
            if k.len() != 8 {
                continue;
            }
            let key_val = u64::from_be_bytes(k.try_into().unwrap());
            let salt_key = SaltKey(key_val);
            if salt_key > end_key {
                break;
            }
            let mut data = [0u8; MAX_SALT_VALUE_BYTES];
            let len = v.len().min(MAX_SALT_VALUE_BYTES);
            data[..len].copy_from_slice(&v[..len]);
            result.push((salt_key, SaltValue { data }));
        }
        Ok(result)
    }

    fn metadata(&self, bucket_id: BucketId) -> Result<BucketMeta, Self::Error> {
        let meta_key = bucket_metadata_key(bucket_id);
        let raw = self.value(meta_key)?;

        let mut meta = match raw {
            Some(ref v) => v.try_into().map_err(MdbxSaltError::Salt)?,
            None => BucketMeta::default(),
        };
        meta.used = Some(self.bucket_used_slots(bucket_id)?);
        Ok(meta)
    }

    fn bucket_used_slots(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        if !is_valid_data_bucket(bucket_id) {
            return Ok(0);
        }
        let counts = self.slot_counts.lock().unwrap();
        Ok(counts.get(&bucket_id).copied().unwrap_or(0))
    }

    fn plain_value_fast(&self, _plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        Err(MdbxSaltError::Salt(SaltError::UnsupportedOperation {
            operation: "MdbxSaltStore::plain_value_fast",
        }))
    }
}

impl TrieReader for MdbxSaltStore {
    type Error = MdbxSaltError;

    fn commitment(&self, node_id: NodeId) -> Result<CommitmentBytes, Self::Error> {
        let tx = self.get_or_create_ro_txn()?;
        let key = node_id.to_be_bytes();
        let result: Option<Vec<u8>> = tx.get(self.trie_dbi, &key)?;

        match result {
            Some(v) if v.len() == 64 => {
                let bytes: [u8; 64] = v.try_into().unwrap();
                Ok(bytes)
            }
            _ => Ok(default_commitment(node_id)),
        }
    }

    fn node_entries(
        &self,
        range: Range<NodeId>,
    ) -> Result<Vec<(NodeId, CommitmentBytes)>, Self::Error> {
        let tx = self.get_or_create_ro_txn()?;
        let mut cursor = tx.cursor(self.trie_dbi)?;

        let start_bytes = range.start.to_be_bytes();
        let mut result = Vec::new();

        let iter = cursor.iter_from::<Vec<u8>, Vec<u8>>(&start_bytes).filter_map(|item| item.ok());

        for (k, v) in iter {
            if k.len() != 8 || v.len() != 64 {
                continue;
            }
            let node_id = u64::from_be_bytes(k.try_into().unwrap());
            if node_id >= range.end {
                break;
            }
            let commitment: [u8; 64] = v.try_into().unwrap();
            result.push((node_id, commitment));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use salt::constant::NUM_META_BUCKETS;
    use tempfile::TempDir;

    fn create_test_store() -> (MdbxSaltStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = MdbxSaltStore::new(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn test_state_roundtrip() {
        let (store, _dir) = create_test_store();

        let key = SaltKey::from((NUM_META_BUCKETS as BucketId + 1, 10u64));
        let val = SaltValue::new(&[1; 20], &[2; 32]);

        let updates = StateUpdates { data: [(key, (None, Some(val.clone())))].into() };
        store.update_state(updates).unwrap();

        let retrieved = store.value(key).unwrap().unwrap();
        assert_eq!(retrieved.data_len(), val.data_len());
        assert_eq!(&retrieved.data[..val.data_len()], &val.data[..val.data_len()]);
    }

    #[test]
    fn test_trie_roundtrip() {
        let (store, _dir) = create_test_store();

        let node_id: NodeId = 42;
        let commitment = [7u8; 64];
        let updates = vec![(node_id, ([0u8; 64], commitment))];
        store.update_trie(updates).unwrap();

        let retrieved = store.commitment(node_id).unwrap();
        assert_eq!(retrieved, commitment);
    }

    #[test]
    fn test_bucket_slots_tracking() {
        let (store, _dir) = create_test_store();

        let bucket_id = NUM_META_BUCKETS as BucketId + 42;
        let key1 = SaltKey::from((bucket_id, 10u64));
        let key2 = SaltKey::from((bucket_id, 20u64));
        let val = SaltValue::new(&[1; 32], &[2; 32]);

        assert_eq!(store.bucket_used_slots(bucket_id).unwrap(), 0);

        let updates = StateUpdates { data: [(key1, (None, Some(val.clone())))].into() };
        store.update_state(updates).unwrap();
        assert_eq!(store.bucket_used_slots(bucket_id).unwrap(), 1);

        let updates = StateUpdates { data: [(key2, (None, Some(val.clone())))].into() };
        store.update_state(updates).unwrap();
        assert_eq!(store.bucket_used_slots(bucket_id).unwrap(), 2);

        let updates = StateUpdates { data: [(key1, (Some(val.clone()), None))].into() };
        store.update_state(updates).unwrap();
        assert_eq!(store.bucket_used_slots(bucket_id).unwrap(), 1);
    }

    #[test]
    fn test_entries_range() {
        let (store, _dir) = create_test_store();

        let bucket_id = NUM_META_BUCKETS as BucketId + 100;
        let val = SaltValue::new(&[1; 20], &[2; 32]);

        let keys: Vec<SaltKey> = (0..5).map(|i| SaltKey::from((bucket_id, i as u64))).collect();

        for key in &keys {
            let updates = StateUpdates { data: [(*key, (None, Some(val.clone())))].into() };
            store.update_state(updates).unwrap();
        }

        let entries = store.entries(keys[1]..=keys[3]).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_default_commitment() {
        let (store, _dir) = create_test_store();
        let node_id: NodeId = 999;
        let commitment = store.commitment(node_id).unwrap();
        assert_eq!(commitment, default_commitment(node_id));
    }

    #[test]
    fn test_full_salt_pipeline() {
        use crate::convert::bundle_state_to_plain_kv;
        use salt::{EphemeralSaltState, StateRoot as SaltStateRoot};

        let (store, _dir) = create_test_store();

        let mut state = alloy_primitives::map::HashMap::default();
        let addr = alloy_primitives::Address::from([0x01; 20]);
        let info = revm_state::AccountInfo {
            nonce: 1,
            balance: alloy_primitives::U256::from(1000u64),
            code_hash: alloy_consensus::constants::KECCAK_EMPTY,
            account_id: None,
            code: None,
        };
        let mut storage = revm_database::StorageWithOriginalValues::default();
        storage.insert(
            alloy_primitives::B256::from([0xaa; 32]).into(),
            revm_database::states::StorageSlot::new_changed(
                alloy_primitives::U256::ZERO,
                alloy_primitives::U256::from(42u64),
            ),
        );
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info),
                original_info: None,
                status: revm_database::AccountStatus::Changed,
                storage,
            },
        );
        let bundle = revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        };

        let kvs = bundle_state_to_plain_kv(&bundle);
        let mut ephemeral = EphemeralSaltState::new(&store);
        let state_updates = ephemeral.update_fin(&kvs).unwrap();
        store.update_state(state_updates.clone()).unwrap();

        let mut root = SaltStateRoot::new(&store);
        let (root_hash, trie_updates) = root.update_fin(&state_updates).unwrap();
        store.update_trie(trie_updates).unwrap();

        assert_ne!(root_hash, [0u8; 32]);
    }

    #[test]
    fn test_ro_cache_invalidation() {
        let (store, _dir) = create_test_store();

        let bucket_id = NUM_META_BUCKETS as BucketId + 1;
        let key = SaltKey::from((bucket_id, 5u64));
        let val = SaltValue::new(&[0xab; 20], &[0xcd; 32]);

        assert!(store.value(key).unwrap().is_none());

        let updates = StateUpdates { data: [(key, (None, Some(val.clone())))].into() };
        store.update_state(updates).unwrap();

        // After write, cached RO txn is invalidated; next read sees committed data
        let retrieved = store.value(key).unwrap();
        assert!(retrieved.is_some());
    }
}
