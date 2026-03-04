//! AsyncRocksStore: in-memory reads + async RocksDB persistence.
//!
//! Combines fast per-bucket in-memory reads with RocksDB (production-grade
//! crash recovery via WAL). Writes update the in-memory state synchronously,
//! then send updates to a background thread for async RocksDB persistence.
//!
//! ## Design
//!
//! - **Reads**: Served from per-bucket `HashMap<BucketId, Vec<(SaltKey, SaltValue)>>`. Each
//!   bucket's Vec is sorted by SaltKey, enabling O(1) bucket lookup + O(log k) binary search within
//!   the bucket (k ≈ 256 entries). Far better cache locality than a single BTreeMap with millions
//!   of entries.
//! - **Writes**: Update in-memory state synchronously, then transfer ownership of the raw
//!   `StateUpdates`/`TrieUpdates` to a background thread that builds a RocksDB `WriteBatch`. No
//!   per-entry copying or intermediate allocations on the main thread.
//! - **Cold start**: On `new()`, the full state is loaded from RocksDB into memory.
//! - **Shutdown**: `Drop` sends a `Shutdown` job and joins the writer thread, ensuring all pending
//!   writes are flushed before the store is dropped.
//!
//! ## Error handling
//!
//! Background RocksDB write failures are captured via an atomic error flag. The main
//! thread checks this flag before dispatching new writes and returns an error if a
//! prior write has failed. This prevents silent data loss.
//!
//! ## Performance
//!
//! - Delta phase reads from per-bucket Vecs: O(1) HashMap lookup + O(log 256) binary search, with
//!   bucket data contiguous in memory (L1/L2 cache friendly).
//! - I/O on the critical path is ~0ms (background write), vs 37ms synchronous RocksDB.

use parking_lot::{Mutex, RwLock};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, Options, WriteBatch, WriteOptions, DB,
};
use salt::{
    constant::default_commitment,
    traits::{StateReader, TrieReader},
    types::*,
    StateUpdates, TrieUpdates,
};
use std::{
    collections::{BTreeMap, HashMap},
    ops::{Range, RangeInclusive},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    time::{Duration, Instant},
};

/// Errors from [`AsyncRocksStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum AsyncRocksError {
    /// A background RocksDB write failed.
    #[error("background RocksDB write failed: {0}")]
    BackgroundWriteFailed(String),
    /// The background writer thread has exited (channel disconnected).
    #[error("background writer thread is dead, channel disconnected")]
    WriterDead,
    /// The background writer has been explicitly shut down.
    #[error("background writer has been shut down")]
    WriterShutdown,
    /// Underlying RocksDB error.
    #[error(transparent)]
    RocksDb(#[from] rocksdb::Error),
}

const CF_STATE: &str = "salt_state";
const CF_TRIE: &str = "salt_trie";
const CF_INDEX: &str = "salt_index";

/// 512MB block cache — used during cold start loading and as RocksDB internal cache.
const BLOCK_CACHE_SIZE: usize = 512 * 1024 * 1024;

/// Maximum pending write jobs before the main thread blocks (backpressure).
/// At ~56K writes/block, each job is ~5-10MB. 64 jobs ≈ 320-640MB max buffer.
const WRITE_CHANNEL_CAPACITY: usize = 64;

fn bytes_to_salt_value(bytes: &[u8]) -> SaltValue {
    let mut data = [0u8; MAX_SALT_VALUE_BYTES];
    let len = bytes.len().min(MAX_SALT_VALUE_BYTES);
    data[..len].copy_from_slice(&bytes[..len]);
    SaltValue { data }
}

/// Write statistics for a single block's persistence.
#[derive(Debug)]
pub struct WriteStats {
    /// Number of state entries written.
    pub entries: usize,
    /// Bytes written.
    pub bytes_written: usize,
    /// Time to update in-memory state (not disk time — disk writes are async).
    pub persist_duration: Duration,
}

/// Per-bucket in-memory state store.
///
/// Uses `HashMap<BucketId, Vec<(SaltKey, SaltValue)>>` instead of a single BTreeMap.
/// Each bucket's Vec is kept sorted by SaltKey. This gives:
/// - O(1) bucket lookup via HashMap (vs O(log N) BTreeMap traversal for N = millions)
/// - O(log k) binary search within bucket (k ≈ 256, fits in L1/L2 cache)
/// - Contiguous memory layout per bucket → excellent cache locality for range scans
#[derive(Default, Clone, Debug)]
struct StateStore {
    /// Per-bucket sorted entries. Each Vec is sorted by SaltKey.
    buckets: HashMap<BucketId, Vec<(SaltKey, SaltValue)>>,
    /// Used slot counts per data bucket.
    used_slots: HashMap<BucketId, u64>,
}

impl StateStore {
    /// Point lookup: O(1) bucket find + O(log k) binary search.
    #[inline]
    fn get(&self, key: &SaltKey) -> Option<&SaltValue> {
        self.buckets.get(&key.bucket_id()).and_then(|bucket| {
            bucket.binary_search_by_key(key, |(k, _)| *k).ok().map(|idx| &bucket[idx].1)
        })
    }

    /// Range scan within a single bucket: O(1) bucket find + O(log k) for boundaries.
    fn range(&self, range: &RangeInclusive<SaltKey>) -> Vec<(SaltKey, SaltValue)> {
        let bucket_id = range.start().bucket_id();
        match self.buckets.get(&bucket_id) {
            Some(bucket) => {
                let start = bucket.partition_point(|(k, _)| k < range.start());
                let end = bucket.partition_point(|(k, _)| k <= range.end());
                bucket[start..end].iter().map(|(k, v)| (*k, v.clone())).collect()
            }
            None => Vec::new(),
        }
    }

    /// Insert or update a key. Maintains sorted order within the bucket's Vec.
    #[inline]
    fn insert(&mut self, key: SaltKey, val: SaltValue) {
        let bucket = self.buckets.entry(key.bucket_id()).or_default();
        match bucket.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(idx) => bucket[idx].1 = val,
            Err(idx) => bucket.insert(idx, (key, val)),
        }
    }

    /// Remove a key. Returns whether the key existed.
    #[inline]
    fn remove(&mut self, key: &SaltKey) -> bool {
        if let Some(bucket) = self.buckets.get_mut(&key.bucket_id()) {
            if let Ok(idx) = bucket.binary_search_by_key(key, |(k, _)| *k) {
                bucket.remove(idx);
                return true;
            }
        }
        false
    }

    /// Check if any entries exist.
    fn is_empty(&self) -> bool {
        self.buckets.values().all(|b| b.is_empty())
    }

    /// Batch-optimized write: exploits the fact that `updates.data` (BTreeMap) is sorted
    /// by SaltKey, so entries from the same bucket are consecutive.
    fn apply_updates(&mut self, updates: &StateUpdates) -> (usize, usize) {
        let mut bytes = 0usize;

        for (key, (old_value, new_value)) in &updates.data {
            let bid = key.bucket_id();

            if !key.is_in_meta_bucket() {
                let delta: i64 = match (old_value.is_some(), new_value.is_some()) {
                    (false, true) => 1,
                    (true, false) => -1,
                    _ => 0,
                };
                if delta != 0 {
                    let count = self.used_slots.entry(bid).or_insert(0);
                    *count = (*count as i64 + delta).max(0) as u64;
                }
            }

            let bucket = self.buckets.entry(bid).or_default();

            match new_value {
                Some(val) => {
                    bytes += 8 + val.data_len();
                    match bucket.binary_search_by_key(key, |(k, _)| *k) {
                        Ok(idx) => bucket[idx].1 = val.clone(),
                        Err(idx) => bucket.insert(idx, (*key, val.clone())),
                    }
                }
                None => {
                    bytes += 8;
                    if let Ok(idx) = bucket.binary_search_by_key(key, |(k, _)| *k) {
                        bucket.remove(idx);
                    }
                }
            }
        }

        (updates.data.len(), bytes)
    }
}

/// Read session: holds the state read lock for the entire delta phase,
/// eliminating per-call RwLock acquire/release overhead (~25K calls × ~70ns = ~1.75ms).
///
/// Usage:
/// ```ignore
/// let session = store.read_session();
/// let mut eph = EphemeralSaltState::new(&session);
/// let state_updates = eph.update_fin(&kvs).unwrap();
/// drop(session); // release before write phase
/// ```
pub struct AsyncRocksReadSession<'a> {
    state: parking_lot::RwLockReadGuard<'a, StateStore>,
    trie: &'a RwLock<BTreeMap<NodeId, CommitmentBytes>>,
    db: &'a Arc<DB>,
}

// SAFETY: All fields provide thread-safe read access:
// - `state` (RwLockReadGuard): immutable shared read, no data races possible
// - `trie` (&RwLock<...>): RwLock is Sync, concurrent read locks are safe
// - `db` (&Arc<DB>): RocksDB handles concurrent reads internally
unsafe impl Send for AsyncRocksReadSession<'_> {}
unsafe impl Sync for AsyncRocksReadSession<'_> {}

impl std::fmt::Debug for AsyncRocksReadSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncRocksReadSession").finish()
    }
}

impl StateReader for AsyncRocksReadSession<'_> {
    type Error = SaltError;

    #[inline]
    fn value(&self, key: SaltKey) -> Result<Option<SaltValue>, Self::Error> {
        Ok(self.state.get(&key).cloned())
    }

    #[inline]
    fn entries(
        &self,
        range: RangeInclusive<SaltKey>,
    ) -> Result<Vec<(SaltKey, SaltValue)>, Self::Error> {
        Ok(self.state.range(&range))
    }

    #[inline]
    fn metadata(&self, bucket_id: BucketId) -> Result<BucketMeta, Self::Error> {
        let key = bucket_metadata_key(bucket_id);
        let mut meta = match self.state.get(&key) {
            Some(v) => v.try_into()?,
            None => BucketMeta::default(),
        };
        meta.used = Some(*self.state.used_slots.get(&bucket_id).unwrap_or(&0));
        Ok(meta)
    }

    #[inline]
    fn bucket_used_slots(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        if !is_valid_data_bucket(bucket_id) {
            return Ok(0);
        }
        Ok(*self.state.used_slots.get(&bucket_id).unwrap_or(&0))
    }

    fn plain_value_fast(&self, plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        let cf = self.db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");
        match self.db.get_cf(&cf, plain_key) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                let arr: [u8; 8] = bytes[..8].try_into().expect("length checked above");
                Ok(SaltKey(u64::from_be_bytes(arr)))
            }
            _ => Err(SaltError::UnsupportedOperation {
                operation: "AsyncRocksReadSession::plain_value_fast: key not in index",
            }),
        }
    }
}

impl TrieReader for AsyncRocksReadSession<'_> {
    type Error = SaltError;

    fn commitment(&self, node_id: NodeId) -> Result<CommitmentBytes, Self::Error> {
        Ok(self.trie.read().get(&node_id).copied().unwrap_or_else(|| default_commitment(node_id)))
    }

    fn node_entries(
        &self,
        range: Range<NodeId>,
    ) -> Result<Vec<(NodeId, CommitmentBytes)>, Self::Error> {
        Ok(self.trie.read().range(range).map(|(k, v)| (*k, *v)).collect())
    }
}

/// A job sent to the background writer thread.
enum WriteJob {
    /// Raw state + trie updates to persist to RocksDB.
    StateAndTrie { state: StateUpdates, trie: TrieUpdates },
    /// Arc-shared state updates (zero-copy dispatch from main thread) + owned trie updates.
    SharedState { state: Arc<StateUpdates>, trie: TrieUpdates },
    /// Barrier: writer sends acknowledgment after processing all prior jobs.
    Sync { done: mpsc::Sender<()> },
    /// Signals the writer thread to exit.
    Shutdown,
}

/// Opaque snapshot of an [`AsyncRocksStore`]'s in-memory state, used for fast reset
/// in benchmarks.
#[derive(Debug)]
pub struct AsyncRocksSnapshot {
    state: StateStore,
    trie: BTreeMap<NodeId, CommitmentBytes>,
}

/// AsyncRocksStore: in-memory reads with background RocksDB persistence.
///
/// All reads are served from per-bucket in-memory Vecs (fast O(1) + O(log k) lookups).
/// All writes update memory synchronously, then are dispatched to a background thread
/// for async RocksDB persistence.
///
/// Background write failures are captured via an atomic error flag and can be checked
/// with [`Self::check_bg_error`].
pub struct AsyncRocksStore {
    /// In-memory state (reads served from here).
    state: RwLock<StateStore>,
    /// In-memory trie commitments.
    trie: RwLock<BTreeMap<NodeId, CommitmentBytes>>,
    /// RocksDB handle (owned by background writer via Arc).
    db: Arc<DB>,
    /// Block cache — must outlive DB.
    _block_cache: Cache,
    /// Bounded channel sender for background write jobs.
    write_tx: Mutex<Option<SyncSender<WriteJob>>>,
    /// Handle for the background writer thread.
    writer_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set to true if the background writer encounters a RocksDB error.
    bg_error_flag: Arc<AtomicBool>,
    /// Stores the error message from the last background write failure.
    bg_error_msg: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for AsyncRocksStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncRocksStore")
            .field("has_bg_error", &self.bg_error_flag.load(Ordering::Relaxed))
            .finish()
    }
}

impl AsyncRocksStore {
    /// Opens or creates an AsyncRocksStore at the given path.
    ///
    /// On cold start, loads all state and trie data from RocksDB into memory.
    /// Spawns a background writer thread for async persistence.
    pub fn new(path: &Path) -> Result<Self, AsyncRocksError> {
        let block_cache = Cache::new_lru_cache(BLOCK_CACHE_SIZE);

        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_cache(&block_cache);
        table_opts.set_bloom_filter(10.0, false);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_write_buffer_size(64 * 1024 * 1024);
        opts.set_max_write_buffer_number(3);
        opts.set_allow_mmap_reads(true);
        opts.set_block_based_table_factory(&table_opts);

        let mut cf_state_opts = Options::default();
        cf_state_opts.set_block_based_table_factory(&table_opts);

        let mut cf_trie_opts = Options::default();
        cf_trie_opts.set_block_based_table_factory(&table_opts);

        let mut cf_index_opts = Options::default();
        cf_index_opts.set_block_based_table_factory(&table_opts);

        let cf_descriptors = vec![
            ColumnFamilyDescriptor::new(CF_STATE, cf_state_opts),
            ColumnFamilyDescriptor::new(CF_TRIE, cf_trie_opts),
            ColumnFamilyDescriptor::new(CF_INDEX, cf_index_opts),
        ];

        let db = Arc::new(DB::open_cf_descriptors(&opts, path, cf_descriptors)?);

        // Load state + trie from RocksDB into memory (cold start).
        let (state_store, trie_map) = Self::load_from_rocksdb(&db)?;

        // Spawn background writer thread with bounded channel for backpressure.
        let (write_tx, write_rx) = mpsc::sync_channel::<WriteJob>(WRITE_CHANNEL_CAPACITY);
        let bg_error_flag = Arc::new(AtomicBool::new(false));
        let bg_error_msg: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let db_clone = Arc::clone(&db);
        let flag_clone = Arc::clone(&bg_error_flag);
        let msg_clone = Arc::clone(&bg_error_msg);
        let writer_handle = std::thread::Builder::new()
            .name("salt-async-rocks-writer".into())
            .spawn(move || {
                Self::writer_loop(db_clone, write_rx, flag_clone, msg_clone);
            })
            .expect("failed to spawn async rocks writer thread");

        Ok(Self {
            state: RwLock::new(state_store),
            trie: RwLock::new(trie_map),
            db,
            _block_cache: block_cache,
            write_tx: Mutex::new(Some(write_tx)),
            writer_handle: Mutex::new(Some(writer_handle)),
            bg_error_flag,
            bg_error_msg,
        })
    }

    /// Loads all state and trie data from RocksDB into per-bucket in-memory Vecs.
    ///
    /// RocksDB iterator returns keys in big-endian order. Since SaltKey has bucket_id
    /// in the high bits, entries arrive grouped by bucket and sorted within each bucket.
    /// We exploit this by pushing directly (no binary search needed during load).
    fn load_from_rocksdb(
        db: &DB,
    ) -> Result<(StateStore, BTreeMap<NodeId, CommitmentBytes>), rocksdb::Error> {
        let mut state_store = StateStore::default();

        // Load CF_STATE into per-bucket Vecs.
        let cf_state = db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let iter = db.iterator_cf(&cf_state, rocksdb::IteratorMode::Start);
        for item in iter {
            let (k, v) = item?;
            if k.len() == 8 {
                let key_u64 = u64::from_be_bytes(k.as_ref().try_into().unwrap());
                let salt_key = SaltKey(key_u64);
                let salt_value = bytes_to_salt_value(&v);

                if !salt_key.is_in_meta_bucket() {
                    let count = state_store.used_slots.entry(salt_key.bucket_id()).or_insert(0);
                    *count += 1;
                }

                // Iterator is sorted by key → entries within each bucket arrive in order.
                // Just push — no binary search needed.
                state_store
                    .buckets
                    .entry(salt_key.bucket_id())
                    .or_default()
                    .push((salt_key, salt_value));
            }
        }

        // Load CF_TRIE
        let mut trie_map = BTreeMap::new();
        let cf_trie = db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let iter = db.iterator_cf(&cf_trie, rocksdb::IteratorMode::Start);
        for item in iter {
            let (k, v) = item?;
            if k.len() == 8 && v.len() == 64 {
                let node_id = u64::from_be_bytes(k.as_ref().try_into().unwrap());
                let mut commitment = [0u8; 64];
                commitment.copy_from_slice(&v);
                trie_map.insert(node_id, commitment);
            }
        }

        Ok((state_store, trie_map))
    }

    /// Background writer loop. Sets `error_flag` on RocksDB write failure.
    fn writer_loop(
        db: Arc<DB>,
        rx: mpsc::Receiver<WriteJob>,
        error_flag: Arc<AtomicBool>,
        error_msg: Arc<Mutex<Option<String>>>,
    ) {
        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);

        for job in rx {
            match job {
                WriteJob::StateAndTrie { state, trie } => {
                    if let Err(e) = Self::write_batch_owned(&db, state, trie, &write_opts) {
                        tracing::error!(target: "salt::async_rocks", %e, "background RocksDB write failed");
                        error_flag.store(true, Ordering::Release);
                        *error_msg.lock() = Some(e.to_string());
                    }
                }
                WriteJob::SharedState { state, trie } => {
                    if let Err(e) = Self::write_batch_shared(&db, &state, trie, &write_opts) {
                        tracing::error!(target: "salt::async_rocks", %e, "background RocksDB write failed");
                        error_flag.store(true, Ordering::Release);
                        *error_msg.lock() = Some(e.to_string());
                    }
                }
                WriteJob::Sync { done } => {
                    let _ = done.send(());
                }
                WriteJob::Shutdown => break,
            }
        }
    }

    /// Builds and writes a RocksDB batch from owned state + trie updates.
    fn write_batch_owned(
        db: &DB,
        state: StateUpdates,
        trie: TrieUpdates,
        write_opts: &WriteOptions,
    ) -> Result<(), rocksdb::Error> {
        let cf_state = db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let cf_trie = db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let cf_index = db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");

        let mut batch = WriteBatch::default();

        for (key, (old_value, new_value)) in state.data {
            let key_bytes = key.0.to_be_bytes();
            if !key.is_in_meta_bucket() {
                match (&old_value, &new_value) {
                    (None, Some(new_val)) => {
                        batch.put_cf(&cf_index, new_val.key(), &key_bytes);
                    }
                    (Some(old_val), None) => {
                        batch.delete_cf(&cf_index, old_val.key());
                    }
                    _ => {}
                }
            }
            match new_value {
                Some(val) => {
                    let data_len = val.data_len();
                    batch.put_cf(&cf_state, &key_bytes, &val.data[..data_len]);
                }
                None => {
                    batch.delete_cf(&cf_state, &key_bytes);
                }
            }
        }

        for (node_id, (_, new_commitment)) in trie {
            batch.put_cf(&cf_trie, &node_id.to_be_bytes(), &new_commitment);
        }

        db.write_opt(batch, write_opts)
    }

    /// Builds and writes a RocksDB batch from shared (Arc) state + owned trie updates.
    fn write_batch_shared(
        db: &DB,
        state: &StateUpdates,
        trie: TrieUpdates,
        write_opts: &WriteOptions,
    ) -> Result<(), rocksdb::Error> {
        let cf_state = db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let cf_trie = db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let cf_index = db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");

        let mut batch = WriteBatch::default();

        for (key, (old_value, new_value)) in &state.data {
            let key_bytes = key.0.to_be_bytes();
            if !key.is_in_meta_bucket() {
                match (old_value, new_value) {
                    (None, Some(new_val)) => {
                        batch.put_cf(&cf_index, new_val.key(), &key_bytes);
                    }
                    (Some(old_val), None) => {
                        batch.delete_cf(&cf_index, old_val.key());
                    }
                    _ => {}
                }
            }
            match new_value {
                Some(val) => {
                    let data_len = val.data_len();
                    batch.put_cf(&cf_state, &key_bytes, &val.data[..data_len]);
                }
                None => {
                    batch.delete_cf(&cf_state, &key_bytes);
                }
            }
        }

        for (node_id, (_, new_commitment)) in trie {
            batch.put_cf(&cf_trie, &node_id.to_be_bytes(), &new_commitment);
        }

        db.write_opt(batch, write_opts)
    }

    /// Checks if a prior background write has failed.
    /// Returns `Ok(())` if no error, or `Err` with the error message.
    pub fn check_bg_error(&self) -> Result<(), AsyncRocksError> {
        if self.bg_error_flag.load(Ordering::Acquire) {
            let msg = self.bg_error_msg.lock().clone().unwrap_or_else(|| "unknown".into());
            Err(AsyncRocksError::BackgroundWriteFailed(msg))
        } else {
            Ok(())
        }
    }

    /// Sends a write job to the background writer, returning an error if the writer
    /// is dead or a prior write has failed.
    fn dispatch_write(&self, job: WriteJob) -> Result<(), AsyncRocksError> {
        self.check_bg_error()?;
        let guard = self.write_tx.lock();
        match guard.as_ref() {
            Some(tx) => tx.send(job).map_err(|_| AsyncRocksError::WriterDead),
            None => Err(AsyncRocksError::WriterShutdown),
        }
    }

    /// Creates a read session that holds the state read lock for the entire lifetime.
    /// Use this for the delta phase to avoid per-call locking overhead.
    pub fn read_session(&self) -> AsyncRocksReadSession<'_> {
        AsyncRocksReadSession { state: self.state.read(), trie: &self.trie, db: &self.db }
    }

    /// Captures a snapshot of the in-memory state + trie for fast restore in benchmarks.
    pub fn snapshot(&self) -> AsyncRocksSnapshot {
        AsyncRocksSnapshot { state: self.state.read().clone(), trie: self.trie.read().clone() }
    }

    /// Restores from a snapshot (in-memory only — does NOT update RocksDB).
    pub fn restore(&self, snap: &AsyncRocksSnapshot) {
        *self.state.write() = snap.state.clone();
        *self.trie.write() = snap.trie.clone();
    }

    /// Dispatches `Arc<StateUpdates>` to the background RocksDB writer thread.
    ///
    /// Cost: ~ns (Arc refcount bump + channel send). No in-memory state mutation.
    pub fn dispatch_state_to_bg(&self, updates: Arc<StateUpdates>) -> Result<(), AsyncRocksError> {
        self.dispatch_write(WriteJob::SharedState { state: updates, trie: TrieUpdates::default() })
    }

    /// Updates only the in-memory per-bucket state. Does **not** dispatch to background.
    pub fn apply_state_in_memory(&self, updates: &StateUpdates) -> WriteStats {
        let t0 = Instant::now();
        let (entries, bytes) = self.state.write().apply_updates(updates);
        WriteStats { entries, bytes_written: bytes, persist_duration: t0.elapsed() }
    }

    /// Applies state updates: updates in-memory state synchronously, then transfers
    /// ownership of the raw `StateUpdates` to the background writer.
    pub fn update_state(&self, updates: StateUpdates) -> Result<WriteStats, AsyncRocksError> {
        let t0 = Instant::now();
        let (entries, bytes) = self.state.write().apply_updates(&updates);
        let persist_duration = t0.elapsed();

        self.dispatch_write(WriteJob::StateAndTrie {
            state: updates,
            trie: TrieUpdates::default(),
        })?;

        Ok(WriteStats { entries, bytes_written: bytes, persist_duration })
    }

    /// Applies trie updates: updates in-memory trie synchronously, then transfers
    /// ownership to the background writer.
    pub fn update_trie(&self, updates: TrieUpdates) -> Result<usize, AsyncRocksError> {
        let count = updates.len();

        {
            let mut trie = self.trie.write();
            for (node_id, (_, new_commitment)) in &updates {
                trie.insert(*node_id, *new_commitment);
            }
        }

        self.dispatch_write(WriteJob::StateAndTrie {
            state: StateUpdates { data: BTreeMap::new() },
            trie: updates,
        })?;

        Ok(count)
    }

    /// Applies state and trie updates together.
    pub fn update_state_and_trie(
        &self,
        state_updates: StateUpdates,
        trie_updates: TrieUpdates,
    ) -> Result<(WriteStats, usize), AsyncRocksError> {
        let t0 = Instant::now();
        let trie_count = trie_updates.len();

        let (entries, bytes) = self.state.write().apply_updates(&state_updates);

        {
            let mut trie = self.trie.write();
            for (node_id, (_, new_commitment)) in &trie_updates {
                trie.insert(*node_id, *new_commitment);
            }
        }

        let persist_duration = t0.elapsed();

        self.dispatch_write(WriteJob::StateAndTrie { state: state_updates, trie: trie_updates })?;

        Ok((WriteStats { entries, bytes_written: bytes, persist_duration }, trie_count))
    }

    /// Blocks until all pending background writes are flushed to RocksDB.
    /// After this call, the writer thread is stopped — the store can no longer persist.
    pub fn flush(&self) {
        let tx = self.write_tx.lock().take();
        if let Some(tx) = tx {
            let _ = tx.send(WriteJob::Shutdown);
        }
        let handle = self.writer_handle.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// Blocks until all pending background writes complete, without stopping the writer.
    pub fn wait_for_idle(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        {
            let guard = self.write_tx.lock();
            if let Some(tx) = guard.as_ref() {
                if tx.send(WriteJob::Sync { done: done_tx }).is_err() {
                    return;
                }
            }
        }
        let _ = done_rx.recv();
    }

    /// Logs bucket used-slot distribution.
    pub fn log_bucket_load_stats(&self) {
        let state = self.state.read();
        let used: Vec<u64> = state.used_slots.values().copied().filter(|&u| u > 0).collect();
        drop(state);
        if used.is_empty() {
            eprintln!("  [SALT] bucket load: 0 buckets with data");
            return;
        }
        let n = used.len();
        let max_used = *used.iter().max().unwrap_or(&0);
        let sum: u64 = used.iter().sum();
        let mean = sum as f64 / n as f64;
        let mut sorted = used.clone();
        sorted.sort_unstable();
        let p50 = sorted[n / 2];
        let idx99 = (n * 99) / 100;
        let p99 = sorted.get(idx99).copied().unwrap_or(p50);
        eprintln!(
            "  [SALT] bucket load: {} buckets with data, used_slots max={} mean={:.1} p50={} p99={}",
            n, max_used, mean, p50, p99
        );
    }
}

impl Drop for AsyncRocksStore {
    fn drop(&mut self) {
        let tx = self.write_tx.lock().take();
        if let Some(tx) = tx {
            let _ = tx.send(WriteJob::Shutdown);
        }
        let handle = self.writer_handle.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl StateReader for AsyncRocksStore {
    type Error = SaltError;

    fn value(&self, key: SaltKey) -> Result<Option<SaltValue>, Self::Error> {
        Ok(self.state.read().get(&key).cloned())
    }

    fn entries(
        &self,
        range: RangeInclusive<SaltKey>,
    ) -> Result<Vec<(SaltKey, SaltValue)>, Self::Error> {
        Ok(self.state.read().range(&range))
    }

    fn metadata(&self, bucket_id: BucketId) -> Result<BucketMeta, Self::Error> {
        let key = bucket_metadata_key(bucket_id);
        let state = self.state.read();
        let mut meta = match state.get(&key) {
            Some(v) => v.try_into()?,
            None => BucketMeta::default(),
        };
        meta.used = Some(*state.used_slots.get(&bucket_id).unwrap_or(&0));
        Ok(meta)
    }

    fn bucket_used_slots(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        if !is_valid_data_bucket(bucket_id) {
            return Ok(0);
        }
        Ok(*self.state.read().used_slots.get(&bucket_id).unwrap_or(&0))
    }

    /// O(1) lookup via RocksDB's reverse index (`CF_INDEX`).
    fn plain_value_fast(&self, plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        let cf = self.db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");
        match self.db.get_cf(&cf, plain_key) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                let arr: [u8; 8] = bytes[..8].try_into().expect("length checked above");
                Ok(SaltKey(u64::from_be_bytes(arr)))
            }
            _ => Err(SaltError::UnsupportedOperation {
                operation: "AsyncRocksStore::plain_value_fast: key not in index",
            }),
        }
    }
}

impl TrieReader for AsyncRocksStore {
    type Error = SaltError;

    fn commitment(&self, node_id: NodeId) -> Result<CommitmentBytes, Self::Error> {
        Ok(self.trie.read().get(&node_id).copied().unwrap_or_else(|| default_commitment(node_id)))
    }

    fn node_entries(
        &self,
        range: Range<NodeId>,
    ) -> Result<Vec<(NodeId, CommitmentBytes)>, Self::Error> {
        Ok(self.trie.read().range(range).map(|(k, v)| (*k, *v)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{account_plain_key, storage_plain_key};
    use alloy_consensus::constants::KECCAK_EMPTY;
    use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
    use revm_state::AccountInfo;
    use salt::{EphemeralSaltState, StateRoot};
    use tempfile::TempDir;

    fn make_bundle(accounts: Vec<(Address, Vec<(B256, U256)>)>) -> revm_database::BundleState {
        let mut state = PrimitivesHashMap::default();
        for (addr, slots) in accounts {
            let info = AccountInfo {
                nonce: 1,
                balance: U256::from(1000u64),
                code_hash: KECCAK_EMPTY,
                account_id: None,
                code: None,
            };
            let mut storage = StorageWithOriginalValues::default();
            for (slot, val) in slots {
                storage.insert(slot.into(), StorageSlot::new_changed(U256::ONE, val));
            }
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(info),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage,
                },
            );
        }
        revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    /// Basic read/write round-trip.
    #[test]
    fn test_basic_read_write() {
        let tmp = TempDir::new().unwrap();
        let store = AsyncRocksStore::new(tmp.path()).unwrap();

        let addr = Address::from([0xAB; 20]);
        let slot = B256::from([0x01; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(42u64))])]);

        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
        let mut eph = EphemeralSaltState::new(&store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        store.update_state(state_updates).unwrap();

        let state = store.state.read();
        assert!(!state.is_empty(), "state should have entries after update");
    }

    /// Cold start: write data, flush, create new instance, verify reads.
    #[test]
    fn test_cold_start_recovery() {
        let tmp = TempDir::new().unwrap();

        let addr = Address::from([0xCD; 20]);
        let slot = B256::from([0x02; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(99u64))])]);

        {
            let store = AsyncRocksStore::new(tmp.path()).unwrap();
            let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
            let mut eph = EphemeralSaltState::new(&store);
            let state_updates = eph.update_fin(&kvs).unwrap();
            store.update_state(state_updates.clone()).unwrap();

            let mut root_engine = StateRoot::new(&store);
            let (_root_bytes, trie_updates) = root_engine.update_fin(&state_updates).unwrap();
            store.update_trie(trie_updates).unwrap();

            store.flush();
        }

        {
            let store = AsyncRocksStore::new(tmp.path()).unwrap();
            let state = store.state.read();
            assert!(!state.is_empty(), "cold-start should load state from RocksDB");
            drop(state);

            let trie = store.trie.read();
            assert!(!trie.is_empty(), "cold-start should load trie from RocksDB");
        }
    }

    /// Full pipeline: state root must be non-zero.
    #[test]
    fn test_full_pipeline() {
        let tmp = TempDir::new().unwrap();
        let store = AsyncRocksStore::new(tmp.path()).unwrap();

        let addr = Address::from([0x11; 20]);
        let slot = B256::from([0x22; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(99u64))])]);

        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
        let mut eph = EphemeralSaltState::new(&store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        store.update_state(state_updates.clone()).unwrap();

        let mut root_engine = StateRoot::new(&store);
        let (root_bytes, trie_updates) = root_engine.update_fin(&state_updates).unwrap();
        store.update_trie(trie_updates).unwrap();

        let root = B256::from(root_bytes);
        assert_ne!(root, B256::ZERO, "state root must be non-zero");
    }

    /// Verify plain_value_fast works after flush + cold start.
    #[test]
    fn test_plain_value_fast() {
        let tmp = TempDir::new().unwrap();

        let addr = Address::from([0xEE; 20]);
        let slot = B256::from([0x03; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(7u64))])]);

        {
            let store = AsyncRocksStore::new(tmp.path()).unwrap();
            let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
            let mut eph = EphemeralSaltState::new(&store);
            let state_updates = eph.update_fin(&kvs).unwrap();
            store.update_state(state_updates).unwrap();
            store.flush();
        }

        {
            let store = AsyncRocksStore::new(tmp.path()).unwrap();
            let acct_key = account_plain_key(&addr);
            let salt_key = store
                .plain_value_fast(&acct_key)
                .expect("account plain_key must be in CF_INDEX after flush + reopen");
            let val = store.value(salt_key).unwrap().expect("SaltKey must exist");
            assert_eq!(val.key(), acct_key.as_slice());

            let storage_key = storage_plain_key(&addr, &slot);
            let salt_key_s = store
                .plain_value_fast(&storage_key)
                .expect("storage plain_key must be in CF_INDEX");
            let val_s = store.value(salt_key_s).unwrap().expect("SaltKey must exist");
            assert_eq!(val_s.key(), storage_key.as_slice());

            let unknown = account_plain_key(&Address::from([0xFF; 20]));
            assert!(store.plain_value_fast(&unknown).is_err());
        }
    }

    /// State root must match FlatFileStore for the same workload.
    #[test]
    fn test_root_matches_flat_store() {
        use crate::flat_store::FlatFileStore;

        let tmp_async = TempDir::new().unwrap();
        let tmp_flat = TempDir::new().unwrap();

        let async_store = AsyncRocksStore::new(tmp_async.path()).unwrap();
        let flat_store = FlatFileStore::new(tmp_flat.path()).unwrap();

        let addr = Address::from([0x33; 20]);
        let bundle = make_bundle(vec![(
            addr,
            vec![
                (B256::from([0x01; 32]), U256::from(100u64)),
                (B256::from([0x02; 32]), U256::from(200u64)),
            ],
        )]);

        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);

        let mut eph_async = EphemeralSaltState::new(&async_store);
        let state_updates_async = eph_async.update_fin(&kvs).unwrap();
        async_store.update_state(state_updates_async.clone()).unwrap();
        let mut root_async = StateRoot::new(&async_store);
        let (root_bytes_async, trie_updates_async) =
            root_async.update_fin(&state_updates_async).unwrap();
        async_store.update_trie(trie_updates_async).unwrap();

        let mut eph_flat = EphemeralSaltState::new(&flat_store);
        let state_updates_flat = eph_flat.update_fin(&kvs).unwrap();
        flat_store.update_state(state_updates_flat.clone()).unwrap();
        let mut root_flat = StateRoot::new(&flat_store);
        let (root_bytes_flat, trie_updates_flat) =
            root_flat.update_fin(&state_updates_flat).unwrap();
        flat_store.update_trie(trie_updates_flat).unwrap();

        assert_eq!(
            root_bytes_async, root_bytes_flat,
            "AsyncRocksStore and FlatFileStore must produce the same state root"
        );
    }

    /// Verify check_bg_error returns Ok when no errors occurred.
    #[test]
    fn test_check_bg_error_ok() {
        let tmp = TempDir::new().unwrap();
        let store = AsyncRocksStore::new(tmp.path()).unwrap();
        assert!(store.check_bg_error().is_ok());
    }

    /// Verify wait_for_idle returns after all pending writes complete.
    #[test]
    fn test_wait_for_idle() {
        let tmp = TempDir::new().unwrap();
        let store = AsyncRocksStore::new(tmp.path()).unwrap();

        let addr = Address::from([0x44; 20]);
        let bundle = make_bundle(vec![(addr, vec![(B256::from([0x01; 32]), U256::from(1u64))])]);
        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
        let mut eph = EphemeralSaltState::new(&store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        store.update_state(state_updates).unwrap();

        store.wait_for_idle();
        assert!(store.check_bg_error().is_ok());
    }
}
