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
use parking_lot::{Mutex, RwLock};
use seidb_common::{
    config::{StateStoreConfig, WalConfig},
    error::{Result, SeiDbError},
};
use seidb_proto::NamedChangeSet;
use seidb_traits::{kv::KvEngine, types::IterOptions, wal::Wal};
use seidb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

/// MVCC database wrapping a KvEngine backend with version-aware key/value storage.
///
/// Supports multi-version concurrency control through MVCC-encoded keys,
/// tombstone-based deletion tracking, and atomic version management.
/// The backend can be RocksDB or MDBX, selected at construction time.
#[allow(dead_code)]
pub struct MvccDatabase {
    pub(crate) engine: Arc<dyn KvEngine>,
    pub(crate) config: StateStoreConfig,
    pub(crate) earliest_version: AtomicI64,
    pub(crate) latest_version: AtomicI64,
    pub(crate) store_key_dirty: RwLock<HashMap<String, i64>>,
    // Initialized by T3.7 (write.rs):
    pub(crate) pending_changes_tx: Option<Sender<VersionedChangesets>>,
    pub(crate) worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub(crate) stream_handler: Option<ChangelogWal>,
}

/// Async write channel message body.
#[allow(dead_code)]
pub(crate) struct VersionedChangesets {
    pub version: i64,
    pub changesets: Vec<NamedChangeSet>,
    pub done: Option<crossbeam_channel::Sender<()>>,
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
            let wal_dir = seidb_wal::utils::log_path(std::path::Path::new(db_dir));
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
            store_key_dirty: RwLock::new(HashMap::new()),
            pending_changes_tx: None,
            worker_handle: Mutex::new(None),
            stream_handler,
        })
    }

    /// Initialize the async writer channel and background thread.
    ///
    /// Must be called after wrapping in `Arc`. Sets up a bounded channel
    /// with capacity `config.async_write_buffer` and spawns a background
    /// thread that consumes from it.
    ///
    /// # Safety
    /// Uses interior pointer mutation to set fields on the Arc'd struct.
    /// Must only be called once, before any concurrent access begins.
    pub fn init_async_writer(self: &Arc<Self>) -> Result<()> {
        let buffer = self.config.async_write_buffer;
        if buffer == 0 {
            return Ok(());
        }

        let (tx, rx) = crossbeam_channel::bounded(buffer);

        // Set the sender via pointer — safe because this is called once before
        // any concurrent readers/writers access these fields.
        let ptr = Arc::as_ptr(self) as *mut MvccDatabase;
        unsafe {
            (*ptr).pending_changes_tx = Some(tx);
        }

        let db_clone = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("mvcc-async-writer".to_string())
            .spawn(move || {
                Self::write_async_in_background(db_clone, rx);
            })
            .map_err(|e| SeiDbError::Other(format!("failed to spawn async writer thread: {e}")))?;

        self.worker_handle.lock().replace(handle);
        Ok(())
    }

    /// Shut down the database: drain the async writer, join the worker thread,
    /// and close the WAL stream handler.
    pub fn close(&mut self) -> Result<()> {
        // Drop the sender to signal the worker to stop.
        drop(self.pending_changes_tx.take());

        // Join the background worker if present.
        if let Some(handle) = self.worker_handle.lock().take() {
            let _ = handle.join();
        }

        // Close the WAL stream handler.
        if let Some(ref mut wal) = self.stream_handler {
            wal.close()?;
        }
        self.stream_handler = None;

        Ok(())
    }

    /// Retrieve the latest version stored in the database.
    /// Returns 0 if no version has been persisted yet.
    fn retrieve_latest_version(engine: &dyn KvEngine) -> Result<i64> {
        match engine.get(LATEST_VERSION_KEY) {
            Ok(Some(bytes)) => {
                if bytes.len() < 8 {
                    return Err(SeiDbError::Other(format!(
                        "latest version value too short: {} bytes",
                        bytes.len()
                    )));
                }
                let arr: [u8; 8] = bytes[..8].try_into().map_err(|_| {
                    SeiDbError::Other("latest version value length mismatch".into())
                })?;
                let val = u64::from_le_bytes(arr);
                if val > i64::MAX as u64 {
                    return Err(SeiDbError::Other(format!("latest version overflows i64: {val}")));
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
                    return Err(SeiDbError::Other(format!(
                        "earliest version value too short: {} bytes",
                        bytes.len()
                    )));
                }
                let arr: [u8; 8] = bytes[..8].try_into().map_err(|_| {
                    SeiDbError::Other("earliest version value length mismatch".into())
                })?;
                let val = u64::from_le_bytes(arr);
                if val > i64::MAX as u64 {
                    return Err(SeiDbError::Other(format!("earliest version overflows i64: {val}")));
                }
                Ok(val as i64)
            }
            Ok(None) => Ok(0),
            Err(e) => Err(e),
        }
    }

    // ── Version management ──────────────────────────────────────────────

    /// Get the latest version (atomic, relaxed ordering).
    pub fn get_latest_version(&self) -> i64 {
        self.latest_version.load(Ordering::Relaxed)
    }

    /// Set the latest version atomically and persist to the database.
    pub fn set_latest_version(&self, version: i64) -> Result<()> {
        self.latest_version.store(version, Ordering::Relaxed);
        let bytes = (version as u64).to_le_bytes();
        self.engine
            .set(LATEST_VERSION_KEY, &bytes, &Default::default())
            .map_err(|e| SeiDbError::Other(format!("set latest version: {e}")))
    }

    /// Get the earliest version (atomic, relaxed ordering).
    pub fn get_earliest_version(&self) -> i64 {
        self.earliest_version.load(Ordering::Relaxed)
    }

    /// Set the earliest version. If `ignore_version` is false, only sets
    /// if the new version is greater than the current earliest. Uses CAS
    /// to avoid races.
    pub fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
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
                        .map_err(|e| SeiDbError::Other(format!("set earliest version: {e}")))?;
                    return Ok(());
                }
                Err(_) => {
                    // CAS failed — retry.
                    continue;
                }
            }
        }
    }

    // ── Read operations ─────────────────────────────────────────────────

    /// Get a value for the given store key and user key at the specified version.
    ///
    /// Returns `Ok(None)` when the key does not exist or has been tombstoned
    /// at the requested version.
    pub fn get(&self, store_key: &str, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if version < self.get_earliest_version() {
            return Ok(None);
        }

        let raw = match Self::get_mvcc_slice(self.engine.as_ref(), store_key, key, version) {
            Ok(v) => v,
            Err(e) => {
                if matches!(e, SeiDbError::RecordNotFound) {
                    return Ok(None);
                }
                return Err(e);
            }
        };

        let (val_bytes, tomb_bytes) = split_mvcc_value(&raw).ok_or_else(|| {
            SeiDbError::Other(format!("invalid MVCC value for store={store_key} key={:?}", key))
        })?;

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
    pub fn has(&self, store_key: &str, version: i64, key: &[u8]) -> Result<bool> {
        Ok(self.get(store_key, version, key)?.is_some())
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Seek the latest MVCC entry for the given key up to (and including)
    /// `version`. Uses seek_lt on the upper bound to find the last entry
    /// in the range, which works correctly with both RocksDB and MDBX.
    fn get_mvcc_slice(
        engine: &dyn KvEngine,
        store_key: &str,
        key: &[u8],
        version: i64,
    ) -> Result<Vec<u8>> {
        let prefixed_key = prepend_store_key(store_key, key);
        let lower = mvcc_encode(&prefixed_key, 0);
        // Upper bound is exclusive, so use version + 1.
        let upper_version = version.saturating_add(1);
        let upper = mvcc_encode(&prefixed_key, upper_version);

        // Don't set upper_bound on the iterator — MDBX's seek_lt doesn't
        // work correctly when the seek target equals the iterator's upper
        // bound. Instead, we seek_lt(upper) and manually verify the result
        // is within our lower bound.
        let iter_opts = IterOptions { lower_bound: Some(lower.clone()), upper_bound: None };

        let mut iter = engine.new_iter(&iter_opts)?;

        // seek_lt positions at the largest key strictly less than `upper`.
        if !iter.seek_lt(&upper) {
            return Err(SeiDbError::RecordNotFound);
        }

        if !iter.valid() {
            return Err(SeiDbError::RecordNotFound);
        }

        // Verify the found key is within our lower bound
        let found_key = iter.key();
        if found_key < lower.as_slice() {
            return Err(SeiDbError::RecordNotFound);
        }

        Ok(iter.value().to_vec())
    }

    /// Returns true if the MVCC-encoded value has a non-empty tombstone suffix.
    #[allow(dead_code)]
    pub(crate) fn val_tombstoned(value: &[u8]) -> bool {
        matches!(split_mvcc_value(value), Some((_, Some(tb))) if !tb.is_empty())
    }

    /// Check if a raw key is a metadata key (prefixed with "s/_").
    #[allow(dead_code)]
    pub(crate) fn is_metadata_key(key: &[u8]) -> bool {
        key.starts_with(b"s/_")
    }

    /// Build the store prefix bytes: `s/k:{store_key}/`.
    pub(crate) fn store_prefix(store_key: &str) -> Vec<u8> {
        format!("s/k:{store_key}/").into_bytes()
    }

    /// Prepend the store key prefix to a user key.
    /// If `store_key` is empty, returns the key unchanged.
    pub(crate) fn prepend_store_key(store_key: &str, key: &[u8]) -> Vec<u8> {
        if store_key.is_empty() {
            return key.to_vec();
        }
        let mut out = Self::store_prefix(store_key);
        out.extend_from_slice(key);
        out
    }

    /// Parse the store name from a prefixed key of the form `s/k:{name}/...`.
    #[allow(dead_code)]
    pub(crate) fn parse_store_key(key: &[u8]) -> Result<String> {
        let key_str = std::str::from_utf8(key)
            .map_err(|e| SeiDbError::Other(format!("invalid utf8: {e}")))?;

        if !key_str.starts_with(PREFIX_STORE) {
            return Err(SeiDbError::Other("not a valid store key".to_string()));
        }

        let after_prefix = &key_str[LEN_PREFIX_STORE..];
        let slash_idx = after_prefix
            .find('/')
            .ok_or_else(|| SeiDbError::Other("not a valid store key".to_string()))?;

        Ok(after_prefix[..slash_idx].to_string())
    }

    /// Compute the lexicographic successor of `b` by incrementing the last
    /// byte. Carries overflow toward the front. Returns `None` if all bytes
    /// are 0xFF (no successor in the same length).
    #[allow(dead_code)]
    pub(crate) fn prefix_end(b: &[u8]) -> Option<Vec<u8>> {
        let mut end = b.to_vec();
        for i in (0..end.len()).rev() {
            if end[i] < 0xFF {
                end[i] += 1;
                end.truncate(i + 1);
                return Some(end);
            }
        }
        None
    }

    /// Expose the inner KvEngine handle.
    pub fn engine(&self) -> &Arc<dyn KvEngine> {
        &self.engine
    }
}

// Free-standing wrappers for use without `Self::` in other modules.
#[allow(dead_code)]
pub(crate) fn prepend_store_key(store_key: &str, key: &[u8]) -> Vec<u8> {
    MvccDatabase::prepend_store_key(store_key, key)
}

#[allow(dead_code)]
pub(crate) fn store_prefix(store_key: &str) -> Vec<u8> {
    MvccDatabase::store_prefix(store_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::encoding::mvcc_encode_value;
    use seidb_traits::types::WriteOptions;
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
    fn write_mvcc(engine: &dyn KvEngine, store_key: &str, key: &[u8], value: &[u8], version: i64) {
        let k = mvcc_encode(&MvccDatabase::prepend_store_key(store_key, key), version);
        let v = mvcc_encode_value(value, 0);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    /// Write an MVCC tombstone directly into the engine.
    fn write_mvcc_tombstone(engine: &dyn KvEngine, store_key: &str, key: &[u8], version: i64) {
        let k = mvcc_encode(&MvccDatabase::prepend_store_key(store_key, key), version);
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

        // Reopen — version should be persisted.
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

        // Try to set to 30 without ignore — should remain 50.
        db.set_earliest_version(30, false).unwrap();
        assert_eq!(db.get_earliest_version(), 50);

        // Set to 60 with ignore_version=true — unconditional override.
        db.set_earliest_version(60, true).unwrap();
        assert_eq!(db.get_earliest_version(), 60);
    }

    #[test]
    fn test_get_set() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "mystore", b"hello", b"world", 1);

        let val = mvcc.get("mystore", 1, b"hello").unwrap();
        assert_eq!(val, Some(b"world".to_vec()));

        // Key that doesn't exist.
        let val = mvcc.get("mystore", 1, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_get_version_aware() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val1", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val2", 2);

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), Some(b"val2".to_vec()));
        // Querying at version 3 should return the latest entry (version 2).
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_get_tombstone() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 1, tombstone at version 2.
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), "store", b"key", 2);

        // At version 1 the value is alive.
        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"alive".to_vec()));
        // At version 2 the tombstone applies — deleted.
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
        // At version 3 the tombstone still applies.
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), None);
    }

    #[test]
    fn test_get_before_earliest() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val", 3);
        mvcc.set_earliest_version(5, false).unwrap();

        // Version 3 is before earliest (5) — should return None.
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), None);
    }

    #[test]
    fn test_has() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val", 1);

        assert!(mvcc.has("store", 1, b"key").unwrap());
        assert!(!mvcc.has("store", 1, b"missing").unwrap());
    }

    #[test]
    fn test_helper_functions() {
        // is_metadata_key
        assert!(MvccDatabase::is_metadata_key(b"s/_latest"));
        assert!(MvccDatabase::is_metadata_key(b"s/_earliest"));
        assert!(!MvccDatabase::is_metadata_key(b"s/k:mystore/key"));

        // store_prefix
        assert_eq!(MvccDatabase::store_prefix("bank"), b"s/k:bank/");

        // prepend_store_key
        assert_eq!(MvccDatabase::prepend_store_key("bank", b"addr1"), b"s/k:bank/addr1");
        // Empty store key returns key as-is.
        assert_eq!(MvccDatabase::prepend_store_key("", b"raw"), b"raw");

        // parse_store_key
        assert_eq!(MvccDatabase::parse_store_key(b"s/k:bank/somekey").unwrap(), "bank");
        assert!(MvccDatabase::parse_store_key(b"invalid").is_err());
        assert!(MvccDatabase::parse_store_key(b"s/k:noSlash").is_err());

        // prefix_end
        assert_eq!(MvccDatabase::prefix_end(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(MvccDatabase::prefix_end(b"ab\xff"), Some(b"ac".to_vec()));
        assert_eq!(MvccDatabase::prefix_end(b"\xff\xff\xff"), None);
        assert_eq!(MvccDatabase::prefix_end(b""), None);
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

    // ── MDBX backend tests ─────────────────────────────────────────────

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

        // Reopen — version should be persisted.
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

        write_mvcc(mvcc.engine.as_ref(), "mystore", b"hello", b"world", 1);

        let val = mvcc.get("mystore", 1, b"hello").unwrap();
        assert_eq!(val, Some(b"world".to_vec()));

        // Key that doesn't exist.
        let val = mvcc.get("mystore", 1, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_mdbx_get_version_aware() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        cfg.use_default_comparer = true;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val1", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val2", 2);

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), Some(b"val2".to_vec()));
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_mdbx_get_tombstone() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.backend = "mdbx".to_string();
        cfg.use_default_comparer = true;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), "store", b"key", 2);

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"alive".to_vec()));
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), None);
    }
}
