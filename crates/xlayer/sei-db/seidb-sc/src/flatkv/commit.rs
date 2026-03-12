use crate::flatkv::{
    keys::{encode_account_value, marshal_local_meta, LocalMeta, DB_LOCAL_META_KEY},
    meta::commit_global_metadata,
    snapshot_dir::{ACCOUNT_DB_DIR, CODE_DB_DIR, LEGACY_DB_DIR, STORAGE_DB_DIR},
    store::CommitStore,
};
use seidb_common::error::{Result, SeiDbError};
use seidb_proto::ChangelogEntry;
use seidb_traits::{kv::KvEngine, types::WriteOptions, wal::Wal};
use tracing::info;

impl CommitStore {
    /// Commits all pending writes to durable storage.
    ///
    /// Sequence:
    /// 1. Write WAL (changelog) — source of truth for crash recovery.
    /// 2. Commit per-DB batches (data + LocalMeta atomically).
    /// 3. Update in-memory committed state.
    /// 4. Persist global metadata watermark.
    /// 5. Clear pending buffers.
    ///
    /// Returns the new committed version.
    pub fn commit(&mut self) -> Result<i64> {
        let version = self.committed_version + 1;

        // Step 1: Write WAL — always sync, source of truth for crash recovery.
        let changelog_entry = ChangelogEntry {
            version,
            changesets: self.pending_change_sets.clone(),
            upgrades: vec![],
        };
        let changelog = self
            .changelog
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("changelog not open".to_string()))?;
        changelog
            .write(changelog_entry)
            .map_err(|e| SeiDbError::Other(format!("changelog write: {e}")))?;

        // Step 2: Commit per-DB batches (data + LocalMeta atomically).
        self.commit_batches(version)?;

        // Step 3: Update in-memory committed state.
        self.committed_version = version;
        self.committed_lt_hash = self.working_lt_hash.clone();

        // Step 4: Persist global metadata (written AFTER per-DB batches so the
        // watermark never exceeds persisted data).
        let metadata_db = self
            .metadata_db
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("metadata_db not open".to_string()))?;
        commit_global_metadata(metadata_db, version, &self.committed_lt_hash, self.config.fsync)?;

        // Step 5: Clear pending buffers.
        self.clear_pending_writes();

        // Snapshot at configured interval.
        if self.config.snapshot_interval > 0 &&
            version % (self.config.snapshot_interval as i64) == 0
        {
            self.write_snapshot()?;
        }

        // Best-effort WAL truncation every 1000 versions.
        if version % 1000 == 0 {
            self.try_truncate_wal();
        }

        info!(version, "FlatKV committed version");
        Ok(version)
    }

    /// Commits pending writes to their respective DBs atomically.
    ///
    /// Each DB batch includes a LocalMeta update so that crash recovery can
    /// determine which DBs have been committed for a given version. DBs with
    /// no pending writes are still updated if the version exceeds their
    /// current LocalMeta (ensures the watermark advances even for empty DBs).
    ///
    /// Also called by catchup to replay WAL without re-writing changelog.
    pub(crate) fn commit_batches(&mut self, version: i64) -> Result<()> {
        let sync_opt = WriteOptions { sync: self.config.fsync };

        // --- accountDB ---
        let account_local_ver =
            self.local_meta.get(ACCOUNT_DB_DIR).map(|m| m.committed_version).unwrap_or(0);
        if !self.account_writes.is_empty() || version > account_local_ver {
            let account_db = self
                .account_db
                .as_ref()
                .ok_or_else(|| SeiDbError::Other("account_db not open".to_string()))?;
            let mut batch = account_db.new_batch();

            for paw in self.account_writes.values() {
                let key = paw.addr.to_vec();
                if paw.is_delete {
                    batch.delete(&key)?;
                } else {
                    let encoded = encode_account_value(&paw.value);
                    batch.set(&key, &encoded)?;
                }
            }

            let new_meta = LocalMeta { committed_version: version };
            batch.set(DB_LOCAL_META_KEY, &marshal_local_meta(&new_meta))?;
            batch.commit(&sync_opt)?;

            self.local_meta.insert(ACCOUNT_DB_DIR.to_string(), new_meta);
        }

        // --- codeDB ---
        let code_local_ver =
            self.local_meta.get(CODE_DB_DIR).map(|m| m.committed_version).unwrap_or(0);
        if !self.code_writes.is_empty() || version > code_local_ver {
            let code_db = self
                .code_db
                .as_ref()
                .ok_or_else(|| SeiDbError::Other("code_db not open".to_string()))?;
            let mut batch = code_db.new_batch();

            for pw in self.code_writes.values() {
                if pw.is_delete {
                    batch.delete(&pw.key)?;
                } else {
                    batch.set(&pw.key, &pw.value)?;
                }
            }

            let new_meta = LocalMeta { committed_version: version };
            batch.set(DB_LOCAL_META_KEY, &marshal_local_meta(&new_meta))?;
            batch.commit(&sync_opt)?;

            self.local_meta.insert(CODE_DB_DIR.to_string(), new_meta);
        }

        // --- storageDB ---
        let storage_local_ver =
            self.local_meta.get(STORAGE_DB_DIR).map(|m| m.committed_version).unwrap_or(0);
        if !self.storage_writes.is_empty() || version > storage_local_ver {
            let storage_db = self
                .storage_db
                .as_ref()
                .ok_or_else(|| SeiDbError::Other("storage_db not open".to_string()))?;
            let mut batch = storage_db.new_batch();

            for pw in self.storage_writes.values() {
                if pw.is_delete {
                    batch.delete(&pw.key)?;
                } else {
                    batch.set(&pw.key, &pw.value)?;
                }
            }

            let new_meta = LocalMeta { committed_version: version };
            batch.set(DB_LOCAL_META_KEY, &marshal_local_meta(&new_meta))?;
            batch.commit(&sync_opt)?;

            self.local_meta.insert(STORAGE_DB_DIR.to_string(), new_meta);
        }

        // --- legacyDB ---
        let legacy_local_ver =
            self.local_meta.get(LEGACY_DB_DIR).map(|m| m.committed_version).unwrap_or(0);
        if !self.legacy_writes.is_empty() || version > legacy_local_ver {
            let legacy_db = self
                .legacy_db
                .as_ref()
                .ok_or_else(|| SeiDbError::Other("legacy_db not open".to_string()))?;
            let mut batch = legacy_db.new_batch();

            for pw in self.legacy_writes.values() {
                if pw.is_delete {
                    batch.delete(&pw.key)?;
                } else {
                    batch.set(&pw.key, &pw.value)?;
                }
            }

            let new_meta = LocalMeta { committed_version: version };
            batch.set(DB_LOCAL_META_KEY, &marshal_local_meta(&new_meta))?;
            batch.commit(&sync_opt)?;

            self.local_meta.insert(LEGACY_DB_DIR.to_string(), new_meta);
        }

        Ok(())
    }

    /// Flushes all 5 database instances to stable storage.
    #[allow(dead_code)]
    pub(crate) fn flush_all_dbs(&self) -> Result<()> {
        if let Some(db) = &self.metadata_db {
            db.flush().map_err(|e| SeiDbError::Other(format!("metadata_db flush: {e}")))?;
        }
        if let Some(db) = &self.account_db {
            db.flush().map_err(|e| SeiDbError::Other(format!("account_db flush: {e}")))?;
        }
        if let Some(db) = &self.code_db {
            db.flush().map_err(|e| SeiDbError::Other(format!("code_db flush: {e}")))?;
        }
        if let Some(db) = &self.storage_db {
            db.flush().map_err(|e| SeiDbError::Other(format!("storage_db flush: {e}")))?;
        }
        if let Some(db) = &self.legacy_db {
            db.flush().map_err(|e| SeiDbError::Other(format!("legacy_db flush: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatkv::{
        keys::{account_key, decode_account_value, ADDRESS_LEN},
        meta::load_local_meta,
    };
    use seidb_common::{
        config::FlatKvConfig,
        evm_keys::{NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
    };
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::TempDir;

    /// Helper: create an open CommitStore.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    fn test_addr(seed: u8) -> [u8; ADDRESS_LEN] {
        let mut addr = [0u8; ADDRESS_LEN];
        addr[0] = seed;
        addr[19] = seed;
        addr
    }

    fn test_slot(seed: u8) -> [u8; 32] {
        let mut slot = [0u8; 32];
        slot[0] = seed;
        slot
    }

    fn make_storage_key(addr: &[u8; ADDRESS_LEN], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN + 32);
        key.push(STATE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    fn make_nonce_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(NONCE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    fn encode_nonce(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    fn evm_cs(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet { name: "evm".to_string(), changeset: Some(ChangeSet { pairs }) }]
    }

    #[test]
    fn test_commit_version_auto_increment() {
        let (mut store, _dir) = open_store();
        assert_eq!(store.version(), 0);

        let v1 = store.commit().unwrap();
        assert_eq!(v1, 1);
        assert_eq!(store.version(), 1);

        let v2 = store.commit().unwrap();
        assert_eq!(v2, 2);
        assert_eq!(store.version(), 2);
    }

    #[test]
    fn test_commit_empty() {
        let (mut store, _dir) = open_store();

        // Empty commit (no pending writes) still increments version.
        let v = store.commit().unwrap();
        assert_eq!(v, 1);
        assert_eq!(store.committed_version, 1);
    }

    #[test]
    fn test_commit_persists_data() {
        let (mut store, _dir) = open_store();

        // Apply a storage write + an account nonce update.
        let addr = test_addr(1);
        let slot = test_slot(0xAA);
        let cs = evm_cs(vec![
            KvPair {
                key: make_storage_key(&addr, &slot),
                value: b"hello_storage".to_vec(),
                delete: false,
            },
            KvPair { key: make_nonce_key(&addr), value: encode_nonce(42), delete: false },
        ]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // Read back from the underlying DB to verify persistence.
        let storage_db = store.storage_db.as_ref().unwrap();
        let mut internal_key = addr.to_vec();
        internal_key.extend_from_slice(&slot);
        let val = storage_db.get(&internal_key).unwrap().expect("storage value missing");
        assert_eq!(val, b"hello_storage");

        let account_db = store.account_db.as_ref().unwrap();
        let raw = account_db.get(&account_key(&addr)).unwrap().expect("account value missing");
        let av = decode_account_value(&raw).unwrap();
        assert_eq!(av.nonce, 42);
    }

    #[test]
    fn test_commit_clears_pending() {
        let (mut store, _dir) = open_store();

        let addr = test_addr(2);
        let slot = test_slot(0xBB);
        let cs = evm_cs(vec![KvPair {
            key: make_storage_key(&addr, &slot),
            value: b"data".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();
        assert!(!store.storage_writes.is_empty());
        assert!(!store.pending_change_sets.is_empty());

        store.commit().unwrap();

        assert!(store.storage_writes.is_empty());
        assert!(store.account_writes.is_empty());
        assert!(store.code_writes.is_empty());
        assert!(store.legacy_writes.is_empty());
        assert!(store.pending_change_sets.is_empty());
    }

    #[test]
    fn test_commit_updates_lt_hash() {
        let (mut store, _dir) = open_store();

        let addr = test_addr(3);
        let slot = test_slot(0xCC);
        let cs = evm_cs(vec![KvPair {
            key: make_storage_key(&addr, &slot),
            value: b"lt_hash_test".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        // After apply, working_lt_hash is updated but committed_lt_hash is not yet.
        let working_before_commit = store.working_lt_hash.clone();
        assert_ne!(store.committed_lt_hash, working_before_commit);

        store.commit().unwrap();

        // After commit, committed_lt_hash should match working_lt_hash.
        assert_eq!(store.committed_lt_hash, working_before_commit);
    }

    #[test]
    fn test_commit_writes_wal() {
        let (mut store, _dir) = open_store();

        let addr = test_addr(4);
        let cs = evm_cs(vec![KvPair {
            key: make_nonce_key(&addr),
            value: encode_nonce(7),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // The WAL should have at least one entry.
        let changelog = store.changelog.as_ref().unwrap();
        let last = changelog.last_offset().unwrap();
        let first = changelog.first_offset().unwrap();
        assert!(last >= first, "WAL should have at least one entry");

        // Read the entry and verify its version.
        let entry = changelog.read_at(last).unwrap();
        assert_eq!(entry.version, 1);
        assert!(!entry.changesets.is_empty());
    }

    #[test]
    fn test_commit_batches_local_meta() {
        let (mut store, _dir) = open_store();

        // Apply writes to account and storage only.
        let addr = test_addr(5);
        let slot = test_slot(0xDD);
        let cs = evm_cs(vec![
            KvPair { key: make_nonce_key(&addr), value: encode_nonce(100), delete: false },
            KvPair {
                key: make_storage_key(&addr, &slot),
                value: b"slot_val".to_vec(),
                delete: false,
            },
        ]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // All 4 data DBs should have LocalMeta at version 1.
        for db_name in &[ACCOUNT_DB_DIR, CODE_DB_DIR, STORAGE_DB_DIR, LEGACY_DB_DIR] {
            let meta = store.local_meta.get(*db_name).expect("local meta missing");
            assert_eq!(meta.committed_version, 1, "{db_name} local meta should be at version 1");

            // Also verify from the DB directly.
            let db = match *db_name {
                "account" => store.account_db.as_ref().unwrap(),
                "code" => store.code_db.as_ref().unwrap(),
                "storage" => store.storage_db.as_ref().unwrap(),
                "legacy" => store.legacy_db.as_ref().unwrap(),
                _ => unreachable!(),
            };
            let persisted_meta = load_local_meta(db).unwrap();
            assert_eq!(
                persisted_meta.committed_version, 1,
                "{db_name} persisted local meta should be at version 1"
            );
        }
    }
}
