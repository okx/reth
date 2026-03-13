use crate::mvcc::{
    batch::MvccRawBatch,
    constants::{IMPORT_COMMIT_BATCH_SIZE, PRUNE_COMMIT_BATCH_SIZE},
    db::MvccDatabase,
    encoding::{decode_uint64_ascending, mvcc_encode, split_mvcc_key, split_mvcc_value},
};
use crossbeam_channel::Receiver;
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::types::{IterOptions, SnapshotNode, WriteOptions};
use std::sync::Arc;

/// Callback type for `raw_iterate`: receives (key, value, version) and returns
/// `true` to stop iteration early.
type RawIterateFn<'a> = &'a mut dyn FnMut(&[u8], &[u8], i64) -> bool;

impl MvccDatabase {
    /// Prune all MVCC entries up to and including the given version.
    ///
    /// Iterates over all entries and removes older versions that are superseded
    /// by a newer version at or below the prune height. Tombstoned entries
    /// whose version is at or below the prune height are physically removed.
    ///
    /// When `config.keep_last_version` is false, even the most recent version
    /// of a key is removed if it falls at or below the prune height.
    ///
    /// After pruning, the earliest version is advanced to `version + 1`.
    pub fn prune(&self, version: i64) -> Result<()> {
        let iter_opts = IterOptions { lower_bound: None, upper_bound: None };
        let mut iter = self.engine.new_iter(&iter_opts)?;
        iter.first();

        let mut batch = self.engine.new_batch();
        let mut counter = 0usize;

        // Tracking state for the previous entry.
        let mut prev_key: Option<Vec<u8>> = None; // user key (without version)
        let mut prev_key_encoded: Option<Vec<u8>> = None; // full MVCC-encoded key
        let mut prev_val_encoded: Option<Vec<u8>> = None;
        let mut prev_version: i64 = 0;
        let mut prev_store: String = String::new();

        while iter.valid() {
            let curr_key_encoded = iter.key().to_vec();
            if curr_key_encoded.is_empty() {
                break;
            }

            // Skip metadata keys (e.g. s/_latest, s/_earliest).
            if Self::is_metadata_key(&curr_key_encoded) {
                iter.next();
                continue;
            }

            // Split the MVCC key into user key and version bytes.
            let (curr_key, curr_version_bytes) = split_mvcc_key(&curr_key_encoded)
                .ok_or_else(|| SeiDbError::Other("invalid MVCC key during prune".to_string()))?;

            let store_key = Self::parse_store_key(curr_key)?;

            // When entering a new store module, check if it needs pruning.
            if store_key != prev_store {
                prev_store = store_key.clone();
                if self.should_skip_store(&store_key) {
                    // Seek past all keys of this store by constructing a key
                    // with the next store name (appending "0" sorts after "/").
                    let next_store = format!("{store_key}0");
                    let next = mvcc_encode(&Self::store_prefix(&next_store), 0);
                    iter.seek_ge(&next);
                    continue;
                }
            }

            let curr_version = match curr_version_bytes {
                Some(vb) => decode_uint64_ascending(vb)?,
                None => 0,
            };

            // Optimisation: if the current version is above the prune height
            // and either keep_last_version is on or the previous entry is also
            // above the prune height, skip to the next user key.
            if curr_version > version && (self.config.keep_last_version || prev_version > version) {
                let mut next_user_key = curr_key.to_vec();
                next_user_key.push(0x00);
                let seek_target = mvcc_encode(&next_user_key, 0);
                iter.seek_ge(&seek_target);
                continue;
            }

            // Core prune logic: decide whether to delete the *previous* entry.
            if let Some(ref prev_enc) = prev_key_encoded &&
                prev_version <= version
            {
                let same_key = prev_key.as_ref().is_some_and(|pk| pk.as_slice() == curr_key);
                let prev_tombstoned =
                    prev_val_encoded.as_ref().is_some_and(|v| Self::val_tombstoned(v));

                if same_key || prev_tombstoned || !self.config.keep_last_version {
                    batch.delete(prev_enc)?;
                    counter += 1;

                    if counter >= PRUNE_COMMIT_BATCH_SIZE {
                        batch.commit(&WriteOptions::default())?;
                        batch = self.engine.new_batch();
                        counter = 0;
                    }
                }
            }

            // Update previous-entry tracking state.
            prev_key = Some(curr_key.to_vec());
            prev_version = curr_version;
            prev_key_encoded = Some(curr_key_encoded);
            prev_val_encoded = if iter.valid() { Some(iter.value().to_vec()) } else { None };

            iter.next();
        }

        // Handle the last entry after the loop ends.
        if let Some(ref prev_enc) = prev_key_encoded &&
            prev_version <= version
        {
            let prev_tombstoned =
                prev_val_encoded.as_ref().is_some_and(|v| Self::val_tombstoned(v));

            if prev_tombstoned || !self.config.keep_last_version {
                batch.delete(prev_enc)?;
                counter += 1;
            }
        }

        // Commit any remaining deletes.
        if counter > 0 {
            batch.commit(&WriteOptions::default())?;
        }

        // Advance the earliest version.
        self.set_earliest_version(version + 1, false)
    }

    /// Returns `true` if the store can be skipped during pruning because it
    /// has not been dirtied since the earliest pruned version.
    fn should_skip_store(&self, store_key: &str) -> bool {
        let dirty = self.store_key_dirty.read();
        match dirty.get(store_key) {
            None => true,
            Some(&dirty_version) => dirty_version < self.get_earliest_version(),
        }
    }

    /// Import snapshot nodes from a channel into the database at the given
    /// version, using multiple worker threads for parallelism.
    pub fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()> {
        let num_workers = self.config.import_num_workers.max(1);
        let engine = Arc::clone(&self.engine);

        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(num_workers);

            for _ in 0..num_workers {
                let rx = nodes.clone();
                let engine_ref = Arc::clone(&engine);
                handles.push(s.spawn(move || Self::import_worker(engine_ref, version, rx)));
            }

            let mut first_err: Option<SeiDbError> = None;
            for h in handles {
                if let Err(e) = h.join().unwrap_or_else(|_| {
                    Err(SeiDbError::Other("import worker panicked".to_string()))
                }) && first_err.is_none()
                {
                    first_err = Some(e);
                }
            }

            if let Some(e) = first_err {
                return Err(e);
            }

            Ok(())
        })?;

        self.set_latest_version(version)
    }

    /// Single import worker: consume nodes from the channel and write in
    /// batches of `IMPORT_COMMIT_BATCH_SIZE`.
    fn import_worker(
        engine: Arc<dyn seidb_traits::kv::KvEngine>,
        version: i64,
        rx: Receiver<SnapshotNode>,
    ) -> Result<()> {
        let mut batch = MvccRawBatch::new(engine.as_ref());
        let mut counter = 0usize;

        for node in rx {
            batch.set(&node.store_key, &node.key, &node.value, version)?;
            counter += 1;

            if counter.is_multiple_of(IMPORT_COMMIT_BATCH_SIZE) {
                batch.write()?;
                batch = MvccRawBatch::new(engine.as_ref());
            }
        }

        if batch.size() > 0 {
            batch.write()?;
        }

        Ok(())
    }

    /// Iterate over all MVCC entries for a given store, invoking the callback
    /// for each non-tombstoned entry.
    ///
    /// The callback receives `(key_without_store_prefix, value, version)`.
    /// If the callback returns `true`, iteration stops early and the method
    /// returns `Ok(true)`. Returns `Ok(false)` when all entries were visited
    /// without the callback requesting a stop.
    pub fn raw_iterate(&self, store_key: &str, f: RawIterateFn<'_>) -> Result<bool> {
        let prefix = Self::store_prefix(store_key);
        let lower = mvcc_encode(&Self::prepend_store_key(store_key, &[]), 0);
        let upper = Self::prefix_end(&prefix);

        let iter_opts = IterOptions { lower_bound: Some(lower), upper_bound: upper };

        let mut iter = self.engine.new_iter(&iter_opts)?;
        iter.first();

        while iter.valid() {
            let curr_key_encoded = iter.key();
            if curr_key_encoded.is_empty() {
                break;
            }

            // Skip metadata keys.
            if Self::is_metadata_key(curr_key_encoded) {
                iter.next();
                continue;
            }

            // Split MVCC key.
            let (user_key, version_bytes) = split_mvcc_key(curr_key_encoded).ok_or_else(|| {
                SeiDbError::Other("invalid MVCC key during raw_iterate".to_string())
            })?;

            // Stop if we've moved past the target store.
            if !store_key.is_empty() && !user_key.starts_with(&prefix) {
                break;
            }

            let curr_version = match version_bytes {
                Some(vb) => decode_uint64_ascending(vb)?,
                None => 0,
            };

            // Decode value and check for tombstone.
            let curr_val_encoded = iter.value();
            if curr_val_encoded.is_empty() {
                break;
            }

            if !Self::val_tombstoned(curr_val_encoded) {
                let (val_bytes, _) = split_mvcc_value(curr_val_encoded).ok_or_else(|| {
                    SeiDbError::Other(format!(
                        "invalid MVCC value during raw_iterate for key {:?}",
                        user_key
                    ))
                })?;

                // Strip the store prefix from the user key.
                let stripped = &user_key[prefix.len()..];

                if f(stripped, val_bytes, curr_version) {
                    return Ok(true);
                }
            }

            iter.next();
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::encoding::mvcc_encode_value;
    use seidb_common::config::StateStoreConfig;
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

    fn test_config_no_keep(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            use_default_comparer: false,
            keep_last_version: false,
            ..Default::default()
        }
    }

    /// Write an MVCC key/value directly into the engine.
    fn write_mvcc(
        engine: &dyn seidb_traits::kv::KvEngine,
        store_key: &str,
        key: &[u8],
        value: &[u8],
        version: i64,
    ) {
        let k = mvcc_encode(&MvccDatabase::prepend_store_key(store_key, key), version);
        let v = mvcc_encode_value(value, 0);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    /// Write an MVCC tombstone directly into the engine.
    fn write_mvcc_tombstone(
        engine: &dyn seidb_traits::kv::KvEngine,
        store_key: &str,
        key: &[u8],
        version: i64,
    ) {
        use crate::mvcc::constants::TOMBSTONE_VAL;
        let k = mvcc_encode(&MvccDatabase::prepend_store_key(store_key, key), version);
        let v = mvcc_encode_value(TOMBSTONE_VAL, version);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    // ── Prune tests ──────────────────────────────────────────────────────

    #[test]
    fn test_prune_basic() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Mark store as dirty so prune doesn't skip it.
        mvcc.store_key_dirty.write().insert("store".to_string(), 1);

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"v1", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"v2", 2);
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"v3", 3);

        mvcc.prune(2).unwrap();

        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"v3".to_vec()));

        // Write a new version and verify the chain is still consistent.
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"v4", 4);
        assert_eq!(mvcc.get("store", 4, b"key").unwrap(), Some(b"v4".to_vec()));
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_prune_tombstone_physical_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        mvcc.store_key_dirty.write().insert("store".to_string(), 1);

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), "store", b"key", 2);

        mvcc.prune(2).unwrap();

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), None);
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), None);
    }

    #[test]
    fn test_prune_keep_last_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path()); // keep_last_version=true
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        mvcc.store_key_dirty.write().insert("store".to_string(), 1);

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"old", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"kept", 2);
        write_mvcc(mvcc.engine.as_ref(), "store", b"solo", b"only", 2);

        mvcc.prune(2).unwrap();

        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"kept".to_vec()));
        assert_eq!(mvcc.get("store", 3, b"solo").unwrap(), Some(b"only".to_vec()));
    }

    #[test]
    fn test_prune_no_keep_last_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config_no_keep(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        mvcc.store_key_dirty.write().insert("store".to_string(), 1);

        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"only", 2);

        mvcc.prune(2).unwrap();

        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
    }

    #[test]
    fn test_prune_batch_commit() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config_no_keep(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        mvcc.store_key_dirty.write().insert("store".to_string(), 1);

        // Write more entries than PRUNE_COMMIT_BATCH_SIZE (50).
        for i in 0..100u32 {
            let key = format!("key_{i:04}");
            write_mvcc(mvcc.engine.as_ref(), "store", key.as_bytes(), b"val", 1);
        }

        mvcc.prune(1).unwrap();

        // All entries should be pruned (keep_last_version=false).
        for i in 0..100u32 {
            let key = format!("key_{i:04}");
            assert_eq!(
                mvcc.get("store", 1, key.as_bytes()).unwrap(),
                None,
                "key {key} should be pruned"
            );
        }
    }

    #[test]
    fn test_prune_advances_earliest_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        mvcc.store_key_dirty.write().insert("store".to_string(), 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"k", b"v", 1);

        assert_eq!(mvcc.get_earliest_version(), 0);

        mvcc.prune(5).unwrap();

        assert_eq!(mvcc.get_earliest_version(), 6);
    }

    #[test]
    fn test_prune_skips_clean_store() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config_no_keep(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Do NOT mark "store" as dirty — it should be skipped during prune.
        write_mvcc(mvcc.engine.as_ref(), "store", b"key", b"val", 5);

        mvcc.prune(3).unwrap();

        assert_eq!(mvcc.get("store", 5, b"key").unwrap(), Some(b"val".to_vec()));
    }

    // ── Import tests ─────────────────────────────────────────────────────

    #[test]
    fn test_import_basic() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(128);

        let producer = std::thread::spawn(move || {
            for i in 0..100u32 {
                tx.send(SnapshotNode {
                    store_key: "store".to_string(),
                    key: format!("key_{i:04}").into_bytes(),
                    value: format!("val_{i}").into_bytes(),
                })
                .unwrap();
            }
        });

        mvcc.import(10, rx).unwrap();
        producer.join().unwrap();

        assert_eq!(mvcc.get_latest_version(), 10);

        for i in 0..100u32 {
            let key = format!("key_{i:04}");
            let expected = format!("val_{i}");
            assert_eq!(
                mvcc.get("store", 10, key.as_bytes()).unwrap(),
                Some(expected.into_bytes()),
                "missing key {key}"
            );
        }
    }

    #[test]
    fn test_import_parallel() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.import_num_workers = 4;
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(256);

        let producer = std::thread::spawn(move || {
            for i in 0..1000u32 {
                tx.send(SnapshotNode {
                    store_key: "store".to_string(),
                    key: format!("key_{i:05}").into_bytes(),
                    value: format!("val_{i}").into_bytes(),
                })
                .unwrap();
            }
        });

        mvcc.import(20, rx).unwrap();
        producer.join().unwrap();

        assert_eq!(mvcc.get_latest_version(), 20);

        for i in 0..1000u32 {
            let key = format!("key_{i:05}");
            let expected = format!("val_{i}");
            assert_eq!(
                mvcc.get("store", 20, key.as_bytes()).unwrap(),
                Some(expected.into_bytes()),
                "missing key {key}"
            );
        }
    }

    // ── RawIterate tests ─────────────────────────────────────────────────

    #[test]
    fn test_raw_iterate() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"a", b"val_a_1", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"a", b"val_a_2", 2);
        write_mvcc(mvcc.engine.as_ref(), "store", b"b", b"val_b_1", 1);

        let mut entries: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
        let stopped_early = mvcc
            .raw_iterate("store", &mut |key, val, ver| {
                entries.push((key.to_vec(), val.to_vec(), ver));
                false // continue iterating
            })
            .unwrap();

        assert!(!stopped_early);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0], (b"a".to_vec(), b"val_a_1".to_vec(), 1));
        assert_eq!(entries[1], (b"a".to_vec(), b"val_a_2".to_vec(), 2));
        assert_eq!(entries[2], (b"b".to_vec(), b"val_b_1".to_vec(), 1));
    }

    #[test]
    fn test_raw_iterate_skip_tombstone() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"a", b"alive", 1);
        write_mvcc_tombstone(mvcc.engine.as_ref(), "store", b"a", 2);
        write_mvcc(mvcc.engine.as_ref(), "store", b"b", b"also_alive", 1);

        let mut entries: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
        mvcc.raw_iterate("store", &mut |key, val, ver| {
            entries.push((key.to_vec(), val.to_vec(), ver));
            false
        })
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (b"a".to_vec(), b"alive".to_vec(), 1));
        assert_eq!(entries[1], (b"b".to_vec(), b"also_alive".to_vec(), 1));
    }

    #[test]
    fn test_raw_iterate_early_stop() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        write_mvcc(mvcc.engine.as_ref(), "store", b"a", b"v1", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"b", b"v2", 1);
        write_mvcc(mvcc.engine.as_ref(), "store", b"c", b"v3", 1);

        let mut count = 0;
        let stopped = mvcc
            .raw_iterate("store", &mut |_key, _val, _ver| {
                count += 1;
                count >= 2 // stop after second entry
            })
            .unwrap();

        assert!(stopped);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_raw_iterate_empty_store() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut count = 0;
        let stopped = mvcc
            .raw_iterate("nonexistent", &mut |_key, _val, _ver| {
                count += 1;
                false
            })
            .unwrap();

        assert!(!stopped);
        assert_eq!(count, 0);
    }
}
