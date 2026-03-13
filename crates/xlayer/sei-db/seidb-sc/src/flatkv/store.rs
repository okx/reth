use crate::flatkv::{
    keys::{AccountValue, Address, LocalMeta},
    lthash::LtHash,
    meta::{load_global_metadata, load_local_meta},
    snapshot_dir::{
        create_working_dir, remove_tmp_dirs, resolve_snapshot_dir, working_dir_path,
        ACCOUNT_DB_DIR, CHANGELOG_DIR, CODE_DB_DIR, FLATKV_ROOT_DIR, LEGACY_DB_DIR, LOCK_FILE_NAME,
        METADATA_DIR, STORAGE_DB_DIR,
    },
};
use fs4::fs_std::FileExt;
use seidb_common::{
    config::{FlatKvConfig, WalConfig},
    error::{Result, SeiDbError},
};
use seidb_engine::engine::RocksDbEngine;
use seidb_proto::NamedChangeSet;
use seidb_traits::kv::KvEngine;
use seidb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use tracing::info;

/// A buffered key-value write for code/storage/legacy DBs.
#[allow(dead_code)]
pub(crate) struct PendingKvWrite {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub is_delete: bool,
}

/// A buffered account write with pre-captured raw value for LtHash delta.
#[allow(dead_code)]
pub(crate) struct PendingAccountWrite {
    pub addr: Address,
    pub value: AccountValue,
    pub is_delete: bool,
    /// Pre-captured encoded value before this write, used for LtHash mix-out.
    pub last_raw_value: Option<Vec<u8>>,
}

/// FlatKV commit store: five RocksDB instances plus a changelog WAL.
///
/// NOT thread-safe; callers must serialize all operations.
#[allow(dead_code)]
pub struct CommitStore {
    pub(crate) config: FlatKvConfig,
    pub(crate) db_dir: String,

    // 5 RocksDB instances (None when closed)
    pub(crate) metadata_db: Option<RocksDbEngine>,
    pub(crate) account_db: Option<RocksDbEngine>,
    pub(crate) code_db: Option<RocksDbEngine>,
    pub(crate) storage_db: Option<RocksDbEngine>,
    pub(crate) legacy_db: Option<RocksDbEngine>,

    // Per-DB local metadata, keyed by DB dir name (e.g. "account").
    pub(crate) local_meta: HashMap<String, LocalMeta>,

    // LtHash state for integrity checking.
    pub(crate) committed_version: i64,
    pub(crate) committed_lt_hash: LtHash,
    pub(crate) working_lt_hash: LtHash,

    // Pending writes buffer (populated by T5.7 write.rs).
    pub(crate) account_writes: HashMap<Vec<u8>, PendingAccountWrite>,
    pub(crate) code_writes: HashMap<Vec<u8>, PendingKvWrite>,
    pub(crate) storage_writes: HashMap<Vec<u8>, PendingKvWrite>,
    pub(crate) legacy_writes: HashMap<Vec<u8>, PendingKvWrite>,

    // WAL
    pub(crate) changelog: Option<ChangelogWal>,
    pub(crate) pending_change_sets: Vec<NamedChangeSet>,

    // Snapshot timing
    pub(crate) last_snapshot_time: Option<std::time::Instant>,

    // File lock prevents multiple processes from opening the same DB.
    pub(crate) file_lock: Option<std::fs::File>,
}

impl CommitStore {
    /// Creates a new (unopened) FlatKV commit store.
    /// Call `load_version` to open and initialize.
    pub fn new(db_dir: &str, config: FlatKvConfig) -> Self {
        Self {
            config,
            db_dir: db_dir.to_string(),
            metadata_db: None,
            account_db: None,
            code_db: None,
            storage_db: None,
            legacy_db: None,
            local_meta: HashMap::new(),
            committed_version: 0,
            committed_lt_hash: LtHash::new(),
            working_lt_hash: LtHash::new(),
            account_writes: HashMap::new(),
            code_writes: HashMap::new(),
            storage_writes: HashMap::new(),
            legacy_writes: HashMap::new(),
            changelog: None,
            pending_change_sets: Vec::new(),
            last_snapshot_time: None,
            file_lock: None,
        }
    }

    /// Opens the store and catches up to the specified version.
    ///
    /// - `target_version == 0`: open latest (follow current symlink + catchup to end of WAL).
    /// - `target_version > 0`: seek the best snapshot <= target, open it, then catchup via WAL to
    ///   reach target_version exactly.
    pub fn load_version(&mut self, target_version: i64) -> Result<()> {
        if target_version == 0 {
            self.open_to(0)
        } else {
            self.open_to(target_version)
        }
    }

    /// Opens all DBs and catches up via WAL to the given version.
    ///
    /// - 0  -> replay to end of WAL (latest).
    /// - >0 -> replay up to (and including) that version.
    fn open_to(&mut self, catchup_target: i64) -> Result<()> {
        self.open()?;

        if catchup_target > 0 && catchup_target != self.committed_version {
            self.catchup(catchup_target)?;
        } else if catchup_target == 0 {
            self.catchup(0)?;
        }

        Ok(())
    }

    /// Opens all database instances.
    ///
    /// Layout:
    /// ```text
    /// flatkv/
    ///   current -> snapshot-NNNNN
    ///   snapshot-NNNNN/{account,code,...}/  (immutable)
    ///   working/{account,code,...}/          (mutable clone)
    ///   changelog/                           (WAL, shared)
    /// ```
    pub(crate) fn open(&mut self) -> Result<()> {
        self.clear_pending_writes();

        let dir = self.flatkv_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| SeiDbError::Other(format!("failed to create base directory: {e}")))?;

        // Acquire file lock if not already held.
        let acquired_lock = if self.file_lock.is_none() {
            self.acquire_file_lock(&dir)?;
            true
        } else {
            false
        };

        // On error, close DBs and release the lock if we acquired it here.
        let result = self.open_inner(&dir);
        if result.is_err() {
            let _ = self.close_dbs_only();
            if acquired_lock {
                self.file_lock = None; // drop releases the lock
            }
        }
        result
    }

    /// Inner open logic, separated so the caller can handle cleanup on error.
    fn open_inner(&mut self, dir: &Path) -> Result<()> {
        remove_tmp_dirs(dir)?;

        let snap_dir = resolve_snapshot_dir(dir)?;
        let work_dir = working_dir_path(dir);
        create_working_dir(&snap_dir, &work_dir)?;

        self.open_all_dbs(&work_dir)?;

        self.load_global_metadata()?;

        info!(dir = %dir.display(), version = self.committed_version, "FlatKV store opened");
        Ok(())
    }

    /// Opens the 5 RocksDB instances, the changelog WAL, and loads per-DB
    /// local metadata. On failure, all already-opened handles are closed via
    /// `close_dbs_only` (caller is responsible for calling it).
    fn open_all_dbs(&mut self, work_dir: &Path) -> Result<()> {
        let sub_dirs = [ACCOUNT_DB_DIR, CODE_DB_DIR, STORAGE_DB_DIR, LEGACY_DB_DIR, METADATA_DIR];

        // Ensure all sub-directories exist.
        for dir_name in &sub_dirs {
            let p = work_dir.join(dir_name);
            fs::create_dir_all(&p).map_err(|e| {
                SeiDbError::Other(format!("failed to create directory {}: {e}", p.display()))
            })?;
        }

        // Open DBs sequentially, assigning to self so close_dbs_only can
        // clean up any already-opened handles on error.
        self.account_db = Some(
            RocksDbEngine::open_plain(&work_dir.join(ACCOUNT_DB_DIR))
                .map_err(|e| SeiDbError::Other(format!("failed to open account: {e}")))?,
        );
        self.code_db = Some(
            RocksDbEngine::open_plain(&work_dir.join(CODE_DB_DIR))
                .map_err(|e| SeiDbError::Other(format!("failed to open code: {e}")))?,
        );
        self.storage_db = Some(
            RocksDbEngine::open_plain(&work_dir.join(STORAGE_DB_DIR))
                .map_err(|e| SeiDbError::Other(format!("failed to open storage: {e}")))?,
        );
        self.legacy_db = Some(
            RocksDbEngine::open_plain(&work_dir.join(LEGACY_DB_DIR))
                .map_err(|e| SeiDbError::Other(format!("failed to open legacy: {e}")))?,
        );
        self.metadata_db = Some(
            RocksDbEngine::open_plain(&work_dir.join(METADATA_DIR))
                .map_err(|e| SeiDbError::Other(format!("failed to open metadata: {e}")))?,
        );

        // Open changelog WAL.
        let flatkv_dir = self.flatkv_dir();
        let changelog_path = flatkv_dir.join(CHANGELOG_DIR);
        self.changelog = Some(
            new_changelog_wal(WalConfig::default(), &changelog_path)
                .map_err(|e| SeiDbError::Other(format!("failed to open changelog: {e}")))?,
        );

        // Load per-DB local metadata.
        let data_db_names = [ACCOUNT_DB_DIR, CODE_DB_DIR, STORAGE_DB_DIR, LEGACY_DB_DIR];
        for name in &data_db_names {
            let db = self
                .db_by_name(name)
                .ok_or_else(|| SeiDbError::Other(format!("{name} DB not open")))?;
            let meta = load_local_meta(db)
                .map_err(|e| SeiDbError::Other(format!("failed to load {name} local meta: {e}")))?;
            self.local_meta.insert(name.to_string(), meta);
        }

        Ok(())
    }

    /// Returns a reference to the named data DB, if open.
    fn db_by_name(&self, name: &str) -> Option<&dyn KvEngine> {
        match name {
            ACCOUNT_DB_DIR => self.account_db.as_ref().map(|db| db as &dyn KvEngine),
            CODE_DB_DIR => self.code_db.as_ref().map(|db| db as &dyn KvEngine),
            STORAGE_DB_DIR => self.storage_db.as_ref().map(|db| db as &dyn KvEngine),
            LEGACY_DB_DIR => self.legacy_db.as_ref().map(|db| db as &dyn KvEngine),
            METADATA_DIR => self.metadata_db.as_ref().map(|db| db as &dyn KvEngine),
            _ => None,
        }
    }

    /// Loads global version and LtHash from the metadata DB.
    fn load_global_metadata(&mut self) -> Result<()> {
        let metadata_db = self
            .metadata_db
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("metadata_db not open".to_string()))?;

        let (version, lt_hash) = load_global_metadata(metadata_db)?;
        self.committed_version = version;
        self.committed_lt_hash = lt_hash.clone();
        self.working_lt_hash = lt_hash;
        Ok(())
    }

    /// Closes all database handles and the WAL but retains the file lock,
    /// preventing a race window during Rollback or LoadVersion.
    pub(crate) fn close_dbs_only(&mut self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        if let Some(mut wal) = self.changelog.take() {
            use seidb_traits::wal::Wal;
            if let Err(e) = wal.close() {
                errors.push(format!("changelog close: {e}"));
            }
        }

        if let Some(mut db) = self.metadata_db.take() &&
            let Err(e) = db.close()
        {
            errors.push(format!("metadataDB close: {e}"));
        }
        if let Some(mut db) = self.storage_db.take() &&
            let Err(e) = db.close()
        {
            errors.push(format!("storageDB close: {e}"));
        }
        if let Some(mut db) = self.code_db.take() &&
            let Err(e) = db.close()
        {
            errors.push(format!("codeDB close: {e}"));
        }
        if let Some(mut db) = self.account_db.take() &&
            let Err(e) = db.close()
        {
            errors.push(format!("accountDB close: {e}"));
        }
        if let Some(mut db) = self.legacy_db.take() &&
            let Err(e) = db.close()
        {
            errors.push(format!("legacyDB close: {e}"));
        }

        self.local_meta.clear();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(SeiDbError::Other(errors.join("; ")))
        }
    }

    /// Closes all database instances and releases the file lock.
    pub fn close(&mut self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }

        let db_err = self.close_dbs_only();

        // Release the file lock (dropping the File releases the OS lock).
        self.file_lock = None;

        if let Err(e) = &db_err {
            return Err(SeiDbError::Other(format!("close error: {e}")));
        }

        info!("FlatKV store closed");
        Ok(())
    }

    /// Reports whether the store's DB handles have been released.
    pub fn is_closed(&self) -> bool {
        self.metadata_db.is_none()
    }

    /// Acquires an exclusive file lock in the given directory to prevent
    /// multiple processes from opening the same store concurrently.
    fn acquire_file_lock(&mut self, dir: &Path) -> Result<()> {
        let lock_path = dir.join(LOCK_FILE_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| SeiDbError::Other(format!("create lock file: {e}")))?;

        file.try_lock_exclusive().map_err(|_| {
            SeiDbError::Other(format!(
                "acquire file lock: already held by another process ({})",
                lock_path.display()
            ))
        })?;

        self.file_lock = Some(file);
        Ok(())
    }

    /// Returns the committed version of the store.
    pub fn version(&self) -> i64 {
        self.committed_version
    }

    /// Returns the Blake3-256 digest of the working LtHash (32 bytes).
    pub fn root_hash(&self) -> Vec<u8> {
        self.working_lt_hash.checksum().to_vec()
    }

    /// Returns the path to the FlatKV root directory.
    pub(crate) fn flatkv_dir(&self) -> PathBuf {
        Path::new(&self.db_dir).join(FLATKV_ROOT_DIR)
    }

    /// Clears all pending write buffers and change sets.
    /// Retains allocated capacity for reuse in the next block.
    pub(crate) fn clear_pending_writes(&mut self) {
        self.account_writes.clear();
        self.code_writes.clear();
        self.storage_writes.clear();
        self.legacy_writes.clear();
        self.pending_change_sets.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        (store, dir)
    }

    #[test]
    fn test_store_new() {
        let (store, _dir) = temp_store();
        assert!(store.is_closed());
        assert!(store.metadata_db.is_none());
        assert!(store.account_db.is_none());
        assert!(store.code_db.is_none());
        assert!(store.storage_db.is_none());
        assert!(store.legacy_db.is_none());
        assert!(store.changelog.is_none());
        assert!(store.file_lock.is_none());
        assert_eq!(store.committed_version, 0);
        assert!(store.local_meta.is_empty());
        assert!(store.account_writes.is_empty());
    }

    #[test]
    fn test_store_open_close() {
        let (mut store, _dir) = temp_store();
        store.load_version(0).unwrap();
        assert!(!store.is_closed());
        assert!(store.metadata_db.is_some());
        assert!(store.account_db.is_some());
        assert!(store.code_db.is_some());
        assert!(store.storage_db.is_some());
        assert!(store.legacy_db.is_some());
        assert!(store.changelog.is_some());
        assert!(store.file_lock.is_some());

        store.close().unwrap();
        assert!(store.is_closed());
        assert!(store.file_lock.is_none());
    }

    #[test]
    fn test_store_version_starts_at_zero() {
        let (mut store, _dir) = temp_store();
        store.load_version(0).unwrap();
        assert_eq!(store.version(), 0);
        store.close().unwrap();
    }

    #[test]
    fn test_store_root_hash_32_bytes() {
        let (mut store, _dir) = temp_store();
        store.load_version(0).unwrap();
        let hash = store.root_hash();
        assert_eq!(hash.len(), 32);
        store.close().unwrap();
    }

    #[test]
    fn test_store_close_idempotent() {
        let (mut store, _dir) = temp_store();
        store.load_version(0).unwrap();
        store.close().unwrap();
        // Second close should not panic.
        store.close().unwrap();
        assert!(store.is_closed());
    }

    #[test]
    fn test_file_lock_prevents_double_open() {
        let dir = TempDir::new().unwrap();
        let db_dir = dir.path().to_str().unwrap();

        let mut store1 = CommitStore::new(db_dir, FlatKvConfig::default());
        store1.load_version(0).unwrap();

        // Second store opening the same directory should fail due to file lock.
        let mut store2 = CommitStore::new(db_dir, FlatKvConfig::default());
        let result = store2.load_version(0);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("lock") || err_msg.contains("already held"),
            "expected lock error, got: {err_msg}"
        );

        store1.close().unwrap();
    }
}
