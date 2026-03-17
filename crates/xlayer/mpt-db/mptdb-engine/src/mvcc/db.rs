use crate::{
    engine::RocksDbEngine,
    mdbx_engine::MdbxEngine,
    mvcc::{
        comparator::{mvcc_comparator_name, mvcc_compare_fn},
        constants::*,
        encoding::{decode_uint64_ascending, mvcc_encode, split_mvcc_value},
    },
};
use crossbeam_channel::Sender;
use mptdb_common::{
    config::{StateStoreConfig, WalConfig},
    error::{MptDbError, Result},
};
use mptdb_proto::ChangeSet;
use mptdb_traits::{kv::KvEngine, types::IterOptions, wal::Wal};
use mptdb_wal::changelog::{new_changelog_wal, ChangelogWal};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc,
};

/// MVCC database wrapping a KvEngine backend with version-aware key/value storage.
///
/// Supports multi-version concurrency control through MVCC-encoded keys,
/// tombstone-based deletion tracking, and atomic version management.
/// The backend can be RocksDB or MDBX, selected at construction time.
///
/// Operates on a single flat key namespace: user keys are MVCC-encoded directly
/// without any store prefix.
#[allow(dead_code)]
pub struct MvccDatabase {
    pub(crate) engine: Arc<dyn KvEngine>,
    pub(crate) config: StateStoreConfig,
    pub(crate) earliest_version: AtomicI64,
    pub(crate) latest_version: AtomicI64,
    /// Tracks the latest version that had any dirty writes, used by prune to
    /// skip work when nothing has changed since the earliest version.
    pub(crate) latest_dirty_version: AtomicI64,
    // Initialized by T3.7 (write.rs):
    pub(crate) pending_changes_tx: Mutex<Option<Sender<VersionedChangesets>>>,
    pub(crate) worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub(crate) stream_handler: Mutex<Option<ChangelogWal>>,
    pub(crate) async_error: AtomicBool,
    pub(crate) async_error_detail: Mutex<Option<String>>,
}

/// Async write channel message body.
#[allow(dead_code)]
pub(crate) struct VersionedChangesets {
    pub version: i64,
    pub changeset: ChangeSet,
    pub done: Option<crossbeam_channel::Sender<Result<()>>>,
}

impl MvccDatabase {
    /// Open the MVCC database at the directory specified in `config`.
    ///
    /// Uses the `backend` config field to select the engine:
    /// - "mdbx" uses MdbxEngine (lexicographic ordering, no custom comparator)
    /// - anything else uses RocksDbEngine with optional MVCC comparator
    pub fn open_db(config: &StateStoreConfig) -> Result<Self> {
        let engine: Arc<dyn KvEngine> = if config.backend == "mdbx" {
            let data_dir = std::path::Path::new(&config.db_directory);
            Arc::new(MdbxEngine::open(data_dir)?)
        } else {
            let data_dir = std::path::Path::new(&config.db_directory);
            let comparator_fn = if config.use_default_comparer {
                None
            } else {
                Some((
                    mvcc_comparator_name().to_string(),
                    Box::new(mvcc_compare_fn)
                        as Box<dyn Fn(&[u8], &[u8]) -> std::cmp::Ordering + Send + Sync>,
                ))
            };
            let rocks_engine = RocksDbEngine::open_mvcc(data_dir, comparator_fn)?;
            Arc::new(rocks_engine)
        };

        Self::open_db_with_engine(engine, config)
    }

    /// Open the MVCC database using a pre-constructed KvEngine backend.
    ///
    /// This allows callers to provide any engine implementing KvEngine.
    pub fn open_db_with_engine(
        engine: Arc<dyn KvEngine>,
        config: &StateStoreConfig,
    ) -> Result<Self> {
        let latest = Self::retrieve_latest_version(engine.as_ref())?;
        let earliest = Self::retrieve_earliest_version(engine.as_ref())?;

        // Optionally initialize the WAL for async writes
        let stream_handler = if config.async_write_buffer > 0 {
            let db_dir = if config.db_directory.is_empty() { "." } else { &config.db_directory };
            let wal_dir = mptdb_wal::utils::log_path(std::path::Path::new(db_dir));
            let wal_config = WalConfig::default();
            Some(new_changelog_wal(wal_config, wal_dir)?)
        } else {
            None
        };

        Ok(Self {
            engine,
            config: config.clone(),
            earliest_version: AtomicI64::new(earliest),
            latest_version: AtomicI64::new(latest),
            latest_dirty_version: AtomicI64::new(0),
            pending_changes_tx: Mutex::new(None),
            worker_handle: Mutex::new(None),
            stream_handler: Mutex::new(stream_handler),
            async_error: AtomicBool::new(false),
            async_error_detail: Mutex::new(None),
        })
    }

    /// Initialize the async writer channel and background thread.
    ///
    /// Must be called after wrapping in `Arc`. Sets up a bounded channel
    /// with capacity `config.async_write_buffer` and spawns a background
    /// thread that consumes changesets and applies them synchronously.
    pub fn init_async_writer(self: &Arc<Self>) -> Result<()> {
        let buffer = self.config.async_write_buffer;
        if buffer == 0 {
            return Ok(());
        }

        let (tx, rx) = crossbeam_channel::bounded(buffer);
        self.pending_changes_tx.lock().replace(tx);

        let db_clone = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("mvcc-async-writer".to_string())
            .spawn(move || {
                Self::write_async_in_background(db_clone, rx);
            })
            .map_err(|e| MptDbError::Other(format!("failed to spawn async writer thread: {e}")))?;

        self.worker_handle.lock().replace(handle);
        Ok(())
    }

    /// Shared shutdown path that works even when the database is wrapped in an `Arc`
    /// and an async writer thread still holds another strong reference.
    pub fn shutdown(&self) -> Result<()> {
        // Drop the sender to signal the worker to stop after draining queued writes.
        let pending_tx = self.pending_changes_tx.lock().take();
        drop(pending_tx);

        // Join the background worker if present.
        let handle = self.worker_handle.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }

        // Close the WAL stream handler.
        let mut wal = self.stream_handler.lock().take();
        if let Some(ref mut wal) = wal {
            wal.close()?;
        }

        self.check_async_error()
    }

    /// Shut down the database: drain the async writer, join the worker thread,
    /// and close the WAL stream handler.
    pub fn close(&mut self) -> Result<()> {
        self.shutdown()
    }

    pub(crate) fn check_async_error(&self) -> Result<()> {
        if self.async_error.load(Ordering::Relaxed) {
            let detail = self.async_error_detail.lock();
            Err(MptDbError::Other(
                detail.clone().unwrap_or_else(|| "mvcc async write failed".to_string()),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn report_async_error(&self, err: &MptDbError) {
        *self.async_error_detail.lock() = Some(format!("mvcc async write failed: {err}"));
        self.async_error.store(true, Ordering::Relaxed);
    }

    /// Retrieve the latest version stored in the database.
    /// Returns 0 if no version has been persisted yet.
    fn retrieve_latest_version(engine: &dyn KvEngine) -> Result<i64> {
        match engine.get(LATEST_VERSION_KEY) {
            Ok(Some(bytes)) => {
                if bytes.len() < 8 {
                    return Err(MptDbError::Other(format!(
                        "latest version value too short: {} bytes",
                        bytes.len()
                    )));
                }
                let arr: [u8; 8] = bytes[..8].try_into().map_err(|_| {
                    MptDbError::Other("latest version value length mismatch".into())
                })?;
                let val = u64::from_le_bytes(arr);
                if val > i64::MAX as u64 {
                    return Err(MptDbError::Other(format!("latest version overflows i64: {val}")));
                }
                Ok(val as i64)
            }
            Ok(None) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Retrieve the earliest version stored in the database.
    /// Returns 0 if no version has been persisted yet.
    fn retrieve_earliest_version(engine: &dyn KvEngine) -> Result<i64> {
        match engine.get(EARLIEST_VERSION_KEY) {
            Ok(Some(bytes)) => {
                if bytes.len() < 8 {
                    return Err(MptDbError::Other(format!(
                        "earliest version value too short: {} bytes",
                        bytes.len()
                    )));
                }
                let arr: [u8; 8] = bytes[..8].try_into().map_err(|_| {
                    MptDbError::Other("earliest version value length mismatch".into())
                })?;
                let val = u64::from_le_bytes(arr);
                if val > i64::MAX as u64 {
                    return Err(MptDbError::Other(format!("earliest version overflows i64: {val}")));
                }
                Ok(val as i64)
            }
            Ok(None) => Ok(0),
            Err(e) => Err(e),
        }
    }

    // -- Version management --------------------------------------------------

    /// Get the latest version (atomic, relaxed ordering).
    pub fn get_latest_version(&self) -> i64 {
        self.latest_version.load(Ordering::Relaxed)
    }

    /// Set the latest version atomically and persist to the database.
    pub fn set_latest_version(&self, version: i64) -> Result<()> {
        self.check_async_error()?;
        self.latest_version.store(version, Ordering::Relaxed);
        let bytes = (version as u64).to_le_bytes();
        self.engine
            .set(LATEST_VERSION_KEY, &bytes, &Default::default())
            .map_err(|e| MptDbError::Other(format!("set latest version: {e}")))
    }

    /// Get the earliest version (atomic, relaxed ordering).
    pub fn get_earliest_version(&self) -> i64 {
        self.earliest_version.load(Ordering::Relaxed)
    }

    /// Set the earliest version. If `ignore_version` is false, only sets
    /// if the new version is greater than the current earliest. Uses CAS
    /// to avoid races.
    pub fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        self.check_async_error()?;
        loop {
            let current = self.earliest_version.load(Ordering::Relaxed);
            if !ignore_version && version <= current {
                return Ok(());
            }
            match self.earliest_version.compare_exchange(
                current,
                version,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let bytes = (version as u64).to_le_bytes();
                    self.engine
                        .set(EARLIEST_VERSION_KEY, &bytes, &Default::default())
                        .map_err(|e| MptDbError::Other(format!("set earliest version: {e}")))?;
                    return Ok(());
                }
                Err(_) => {
                    // CAS failed -- retry.
                    continue;
                }
            }
        }
    }

    // -- Read operations -----------------------------------------------------

    /// Get a value for the given user key at the specified version.
    ///
    /// Returns `Ok(None)` when the key does not exist or has been tombstoned
    /// at the requested version.
    pub fn get(&self, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.check_async_error()?;
        if version < self.get_earliest_version() {
            return Ok(None);
        }

        let raw = match Self::get_mvcc_slice(self.engine.as_ref(), key, version) {
            Ok(v) => v,
            Err(e) => {
                if matches!(e, MptDbError::RecordNotFound) {
                    return Ok(None);
                }
                return Err(e);
            }
        };

        let (val_bytes, tomb_bytes) = split_mvcc_value(&raw)
            .ok_or_else(|| MptDbError::Other(format!("invalid MVCC value for key={:?}", key)))?;

        // No tombstone means the value is live.
        let tomb_bytes = match tomb_bytes {
            Some(tb) if !tb.is_empty() => tb,
            _ => return Ok(Some(val_bytes.to_vec())),
        };

        let tombstone = decode_uint64_ascending(tomb_bytes)?;

        // If the requested version is earlier than the tombstone, the value
        // was still alive at that point.
        if version < tombstone {
            Ok(Some(val_bytes.to_vec()))
        } else {
            Ok(None)
        }
    }

    /// Check whether a key exists at the given version.
    pub fn has(&self, version: i64, key: &[u8]) -> Result<bool> {
        Ok(self.get(version, key)?.is_some())
    }

    // -- Internal helpers ----------------------------------------------------

    /// Seek the latest MVCC entry for the given key up to (and including)
    /// `version`. Uses seek_lt on the upper bound to find the last entry
    /// in the range, which works correctly with both RocksDB and MDBX.
    fn get_mvcc_slice(engine: &dyn KvEngine, key: &[u8], version: i64) -> Result<Vec<u8>> {
        let lower = mvcc_encode(key, 0);
        // Upper bound is exclusive, so use version + 1.
        let upper_version = version.saturating_add(1);
        let upper = mvcc_encode(key, upper_version);

        // Don't set upper_bound on the iterator -- MDBX's seek_lt doesn't
        // work correctly when the seek target equals the iterator's upper
        // bound. Instead, we seek_lt(upper) and manually verify the result
        // is within our lower bound.
        let iter_opts = IterOptions { lower_bound: Some(lower.clone()), upper_bound: None };

        let mut iter = engine.new_iter(&iter_opts)?;

        // seek_lt positions at the largest key strictly less than `upper`.
        if !iter.seek_lt(&upper) {
            return Err(MptDbError::RecordNotFound);
        }

        if !iter.valid() {
            return Err(MptDbError::RecordNotFound);
        }

        // Verify the found key is within our lower bound
        let found_key = iter.key();
        if found_key < lower.as_slice() {
            return Err(MptDbError::RecordNotFound);
        }

        Ok(iter.value().to_vec())
    }

    /// Returns true if the MVCC-encoded value has a non-empty tombstone suffix.
    #[allow(dead_code)]
    pub(crate) fn val_tombstoned(value: &[u8]) -> bool {
        matches!(split_mvcc_value(value), Some((_, Some(tb))) if !tb.is_empty())
    }

    /// Expose the inner KvEngine handle.
    pub fn engine(&self) -> &Arc<dyn KvEngine> {
        &self.engine
    }
}

#[cfg(test)]
impl MvccDatabase {
    pub(crate) fn inject_async_error_for_test(&self, msg: &str) {
        *self.async_error_detail.lock() = Some(msg.to_string());
        self.async_error.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::encoding::mvcc_encode_value;
    use mptdb_traits::types::WriteOptions;
    use tempfile::TempDir;

    /// Helper: build a minimal StateStoreConfig pointing at the given dir.
    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            use_default_comparer: false,
            ..Default::default()
        }
    }

    /// Write an MVCC key/value directly into the engine.
    fn write_mvcc(engine: &dyn KvEngine, key: &[u8], value: &[u8], version: i64) {
        let k = mvcc_encode(key, version);
        let v = mvcc_encode_value(value, 0);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    /// Write an MVCC tombstone directly into the engine.
    fn write_mvcc_tombstone(engine: &dyn KvEngine, key: &[u8], version: i64) {
        let k = mvcc_encode(key, version);
        let v = mvcc_encode_value(TOMBSTONE_VAL, version);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    #[test]
    fn test_open_and_close() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut db = MvccDatabase::open_db(&cfg).unwrap();
        db.close().unwrap();
    }

    #[test]
    fn test_close_idempotent() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut db = MvccDatabase::open_db(&cfg).unwrap();
        db.close().unwrap();
        db.close().unwrap();
    }

    #[test]
    fn test_version_management() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());

        {
            let db = MvccDatabase::open_db(&cfg).unwrap();
            assert_eq!(db.get_latest_version(), 0);
            db.set_latest_version(100).unwrap();
            assert_eq!(db.get_latest_version(), 100);
        }

        // Reopen -- version should be persisted.
        {
            let db = MvccDatabase::open_db(&cfg).unwrap();
            assert_eq!(db.get_latest_version(), 100);
        }
    }

    #[test]
    fn test_earliest_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        // Set earliest to 50 (non-ignore).
        db.set_earliest_version(50, false).unwrap();
        assert_eq!(db.get_earliest_version(), 50);

        // Try to set to 30 without ignore -- should remain 50.
        db.set_earliest_version(30, false).unwrap();
        assert_eq!(db.get_earliest_version(), 50);

        // Set to 60 with ignore_version=true -- unconditional override.
        db.set_earliest_version(60, true).unwrap();
        assert_eq!(db.get_earliest_version(), 60);
    }

    #[test]
    fn test_get_set() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"hello", b"world", 1);

        let val = mvcc.get(1, b"hello").unwrap();
        assert_eq!(val, Some(b"world".to_vec()));

        // Key that doesn't exist.
        let val = mvcc.get(1, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_get_version_aware() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"key", b"val1", 1);
        write_mvcc(mvcc.engine.as_ref(), b"key", b"val2", 2);

        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(mvcc.get(2, b"key").unwrap(), Some(b"val2".to_vec()));
        // Querying at version 3 should return the latest entry (version 2).
        assert_eq!(mvcc.get(3, b"key").unwrap(), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_get_tombstone() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 1, tombstone at version 2.
        write_mvcc(mvcc.engine.as_ref(), b"key", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), b"key", 2);

        // At version 1 the value is alive.
        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"alive".to_vec()));
        // At version 2 the tombstone applies -- deleted.
        assert_eq!(mvcc.get(2, b"key").unwrap(), None);
        // At version 3 the tombstone still applies.
        assert_eq!(mvcc.get(3, b"key").unwrap(), None);
    }

    #[test]
    fn test_get_before_earliest() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"key", b"val", 3);
        mvcc.set_earliest_version(5, false).unwrap();

        // Version 3 is before earliest (5) -- should return None.
        assert_eq!(mvcc.get(3, b"key").unwrap(), None);
    }

    #[test]
    fn test_has() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"key", b"val", 1);

        assert!(mvcc.has(1, b"key").unwrap());
        assert!(!mvcc.has(1, b"missing").unwrap());
    }

    #[test]
    fn test_val_tombstoned() {
        let live = mvcc_encode_value(b"hello", 0);
        assert!(!MvccDatabase::val_tombstoned(&live));

        let dead = mvcc_encode_value(TOMBSTONE_VAL, 5);
        assert!(MvccDatabase::val_tombstoned(&dead));
    }

    #[test]
    fn test_engine_accessor() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();
        // Should be able to access the underlying engine.
        let _engine: &Arc<dyn KvEngine> = mvcc.engine();
    }

    // -- MDBX backend tests --------------------------------------------------

    #[test]
    fn test_mdbx_open_and_close() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        let mut db = MvccDatabase::open_db(&cfg).unwrap();
        db.close().unwrap();
    }

    #[test]
    fn test_mdbx_version_management() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();

        {
            let db = MvccDatabase::open_db(&cfg).unwrap();
            assert_eq!(db.get_latest_version(), 0);
            db.set_latest_version(100).unwrap();
            assert_eq!(db.get_latest_version(), 100);
        }

        // Reopen -- version should be persisted.
        {
            let db = MvccDatabase::open_db(&cfg).unwrap();
            assert_eq!(db.get_latest_version(), 100);
        }
    }

    #[test]
    fn test_mdbx_get_set() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        // MDBX uses lexicographic ordering, so we use default comparer
        cfg.use_default_comparer = true;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"hello", b"world", 1);

        let val = mvcc.get(1, b"hello").unwrap();
        assert_eq!(val, Some(b"world".to_vec()));

        // Key that doesn't exist.
        let val = mvcc.get(1, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_mdbx_get_version_aware() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        cfg.use_default_comparer = true;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"key", b"val1", 1);
        write_mvcc(mvcc.engine.as_ref(), b"key", b"val2", 2);

        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(mvcc.get(2, b"key").unwrap(), Some(b"val2".to_vec()));
        assert_eq!(mvcc.get(3, b"key").unwrap(), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_mdbx_get_tombstone() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        cfg.use_default_comparer = true;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), b"key", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), b"key", 2);

        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"alive".to_vec()));
        assert_eq!(mvcc.get(2, b"key").unwrap(), None);
        assert_eq!(mvcc.get(3, b"key").unwrap(), None);
    }
}
