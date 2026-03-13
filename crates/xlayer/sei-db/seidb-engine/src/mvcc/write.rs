use crate::mvcc::{
    batch::MvccBatch,
    constants::DELETE_COMMIT_BATCH_SIZE,
    db::{MvccDatabase, VersionedChangesets},
    encoding::{decode_uint64_ascending, mvcc_encode, split_mvcc_key},
};
use crossbeam_channel::Receiver;
use seidb_common::error::{Result, SeiDbError};
use seidb_proto::{ChangelogEntry, NamedChangeSet};
use seidb_traits::{types::IterOptions, wal::Wal};
use std::sync::{atomic::Ordering, Arc};

impl MvccDatabase {
    /// Apply a set of named changesets synchronously at `version`.
    ///
    /// Genesis writes arrive as version 0 but PebbleDB/RocksDB treats version 0
    /// as special, so they are remapped to version 1.
    pub fn apply_changeset_sync(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        // Genesis compatibility: remap version 0 -> 1
        let version = if version == 0 { 1 } else { version };

        let mut batch = MvccBatch::new(self.engine.as_ref(), version)?;

        for cs in changesets {
            if let Some(ref changeset) = cs.changeset {
                for kv_pair in &changeset.pairs {
                    if kv_pair.delete {
                        batch.delete(&cs.name, &kv_pair.key)?;
                    } else {
                        batch.set(&cs.name, &kv_pair.key, &kv_pair.value)?;
                    }
                }
            }
            // Mark the store as updated
            self.store_key_dirty.write().insert(cs.name.clone(), version);
        }

        batch.write()?;

        // Update latest version only if higher (avoid lowering on out-of-order writes)
        let current = self.latest_version.load(Ordering::Relaxed);
        if version > current {
            self.latest_version.store(version, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Apply a set of named changesets asynchronously.
    ///
    /// Writes are first durably recorded to the WAL, then enqueued for the
    /// background consumer which calls [`apply_changeset_sync`].
    pub fn apply_changeset_async(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        // Write to WAL for durability
        if let Some(ref wal) = self.stream_handler {
            let entry =
                ChangelogEntry { version, changesets: changesets.to_vec(), upgrades: vec![] };
            wal.write(entry)?;
        }

        // Enqueue for the background writer
        let tx = self
            .pending_changes_tx
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("async writer not initialized".to_string()))?;

        tx.send(VersionedChangesets { version, changesets: changesets.to_vec(), done: None })
            .map_err(|e| SeiDbError::Other(format!("failed to send to async writer: {e}")))?;

        Ok(())
    }

    /// Block until all pending async writes have been processed.
    ///
    /// Sends a barrier message through the channel and waits for the background
    /// consumer to acknowledge it.
    pub fn wait_for_pending_writes(&self) {
        let tx = match self.pending_changes_tx.as_ref() {
            Some(tx) => tx,
            None => return,
        };

        let (done_tx, done_rx) = crossbeam_channel::bounded(0);

        // Send a barrier: version=0, empty changesets, done channel set
        if tx
            .send(VersionedChangesets { version: 0, changesets: vec![], done: Some(done_tx) })
            .is_err()
        {
            // Channel closed — worker already stopped
            return;
        }

        // Block until the background worker processes the barrier
        let _ = done_rx.recv();
    }

    /// Background consumer loop for async writes.
    ///
    /// Reads `VersionedChangesets` from the channel and applies them
    /// synchronously. Barrier messages (with `done` set) are acknowledged
    /// immediately without writing.
    pub(crate) fn write_async_in_background(
        db: Arc<MvccDatabase>,
        rx: Receiver<VersionedChangesets>,
    ) {
        for msg in rx {
            if let Some(done) = msg.done {
                // Barrier message — signal the waiter and continue
                let _ = done.send(());
                continue;
            }

            if let Err(e) = db.apply_changeset_sync(msg.version, &msg.changesets) {
                tracing::warn!(
                    version = msg.version,
                    error = %e,
                    "async write failed"
                );
            }
        }
    }

    /// Physically remove all MVCC entries for `module` at a specific `version`.
    ///
    /// Iterates over the module's key space using a KvEngine iterator,
    /// identifies entries whose MVCC version matches `version`, and hard-deletes
    /// them in batches of [`DELETE_COMMIT_BATCH_SIZE`].
    pub fn delete_keys_at_version(&self, module: &str, version: i64) -> Result<()> {
        let prefix = Self::store_prefix(module);
        let lower = mvcc_encode(&Self::prepend_store_key(module, &[]), 0);
        let upper = Self::prefix_end(&prefix).map(|pe| mvcc_encode(&pe, 0));

        let iter_opts = IterOptions { lower_bound: Some(lower), upper_bound: upper };

        let mut iter = self.engine.new_iter(&iter_opts)?;
        iter.first();

        let mut batch = MvccBatch::new(self.engine.as_ref(), version)?;
        let mut delete_counter = 0usize;

        while iter.valid() {
            let raw_key = iter.key();
            if raw_key.is_empty() {
                break;
            }

            // Skip metadata keys
            if Self::is_metadata_key(raw_key) {
                iter.next();
                continue;
            }

            // Decode the MVCC key to extract the user key and version
            let (user_key, version_bytes) = match split_mvcc_key(raw_key) {
                Some(v) => v,
                None => {
                    iter.next();
                    continue;
                }
            };

            let entry_version = match version_bytes {
                Some(vb) => match decode_uint64_ascending(vb) {
                    Ok(v) => v,
                    Err(_) => {
                        iter.next();
                        continue;
                    }
                },
                None => 0,
            };

            if entry_version == version {
                // Strip the store prefix to get the user key for hard_delete
                let user_key_stripped = if user_key.starts_with(&prefix) {
                    &user_key[prefix.len()..]
                } else {
                    user_key
                };

                batch.hard_delete(module, user_key_stripped)?;
                delete_counter += 1;

                if delete_counter >= DELETE_COMMIT_BATCH_SIZE {
                    batch.write()?;
                    delete_counter = 0;
                    batch = MvccBatch::new(self.engine.as_ref(), version)?;
                }
            }

            iter.next();
        }

        // Commit any remaining deletions
        if batch.size() > 0 {
            batch.write()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::config::StateStoreConfig;
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::TempDir;

    /// Helper: build a minimal StateStoreConfig pointing at the given dir.
    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            use_default_comparer: false,
            ..Default::default()
        }
    }

    /// Helper: build a NamedChangeSet with the given name and key/value pairs.
    fn make_changeset(name: &str, pairs: Vec<KvPair>) -> NamedChangeSet {
        NamedChangeSet { name: name.to_string(), changeset: Some(ChangeSet { pairs }) }
    }

    /// Helper: build a set KvPair.
    fn kv_set(key: &[u8], value: &[u8]) -> KvPair {
        KvPair { delete: false, key: key.to_vec(), value: value.to_vec() }
    }

    /// Helper: build a delete KvPair.
    fn kv_delete(key: &[u8]) -> KvPair {
        KvPair { delete: true, key: key.to_vec(), value: vec![] }
    }

    #[test]
    fn test_apply_changeset_sync() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        let changesets = vec![
            make_changeset("bank", vec![kv_set(b"addr1", b"100"), kv_set(b"addr2", b"200")]),
            make_changeset("staking", vec![kv_set(b"val1", b"power50")]),
        ];

        db.apply_changeset_sync(1, &changesets).unwrap();

        assert_eq!(db.get("bank", 1, b"addr1").unwrap(), Some(b"100".to_vec()));
        assert_eq!(db.get("bank", 1, b"addr2").unwrap(), Some(b"200".to_vec()));
        assert_eq!(db.get("staking", 1, b"val1").unwrap(), Some(b"power50".to_vec()));
        assert_eq!(db.get_latest_version(), 1);
    }

    #[test]
    fn test_apply_changeset_genesis_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        let changesets = vec![make_changeset("bank", vec![kv_set(b"genesis_key", b"genesis_val")])];

        // Version 0 should be remapped to 1
        db.apply_changeset_sync(0, &changesets).unwrap();

        // Should be readable at version 1 (not 0)
        assert_eq!(db.get("bank", 1, b"genesis_key").unwrap(), Some(b"genesis_val".to_vec()));
        assert_eq!(db.get_latest_version(), 1);
    }

    #[test]
    fn test_apply_changeset_with_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        // Write a value at version 1
        let cs1 = vec![make_changeset("bank", vec![kv_set(b"key", b"alive")])];
        db.apply_changeset_sync(1, &cs1).unwrap();
        assert_eq!(db.get("bank", 1, b"key").unwrap(), Some(b"alive".to_vec()));

        // Delete at version 2 via changeset
        let cs2 = vec![make_changeset("bank", vec![kv_delete(b"key")])];
        db.apply_changeset_sync(2, &cs2).unwrap();

        // At version 2 the key should be gone
        assert_eq!(db.get("bank", 2, b"key").unwrap(), None);
        // At version 1 the key should still be alive
        assert_eq!(db.get("bank", 1, b"key").unwrap(), Some(b"alive".to_vec()));
    }

    #[test]
    fn test_store_key_dirty_tracking() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        let changesets = vec![
            make_changeset("bank", vec![kv_set(b"k1", b"v1")]),
            make_changeset("staking", vec![kv_set(b"k2", b"v2")]),
        ];

        db.apply_changeset_sync(5, &changesets).unwrap();

        let dirty = db.store_key_dirty.read();
        assert_eq!(dirty.get("bank"), Some(&5));
        assert_eq!(dirty.get("staking"), Some(&5));
        assert_eq!(dirty.get("missing"), None);
    }

    #[test]
    fn test_delete_keys_at_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        // Write key at version 1 and version 2
        let cs1 = vec![make_changeset("bank", vec![kv_set(b"key", b"v1")])];
        db.apply_changeset_sync(1, &cs1).unwrap();

        let cs2 = vec![make_changeset("bank", vec![kv_set(b"key", b"v2")])];
        db.apply_changeset_sync(2, &cs2).unwrap();

        // Verify both versions exist
        assert_eq!(db.get("bank", 1, b"key").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(db.get("bank", 2, b"key").unwrap(), Some(b"v2".to_vec()));

        // Delete all entries at version 1
        db.delete_keys_at_version("bank", 1).unwrap();

        // Version 1 should be gone
        assert_eq!(db.get("bank", 1, b"key").unwrap(), None);
        // Version 2 should still be present
        assert_eq!(db.get("bank", 2, b"key").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_apply_changeset_async_and_wait() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let db = MvccDatabase::open_db(&cfg).unwrap();

        // Initialize the async writer
        let db = Arc::new(db);
        let (tx, rx) = crossbeam_channel::bounded(16);

        // SAFETY: we manually set the tx field via a second mutable reference.
        // This is safe because we haven't started any concurrent access yet.
        {
            let db_ptr = Arc::as_ptr(&db) as *mut MvccDatabase;
            unsafe {
                (*db_ptr).pending_changes_tx = Some(tx);
            }
        }

        // Spawn the background worker
        let db_clone = Arc::clone(&db);
        let handle = std::thread::spawn(move || {
            MvccDatabase::write_async_in_background(db_clone, rx);
        });

        // Send an async changeset (no WAL in this test)
        let changesets = vec![make_changeset("bank", vec![kv_set(b"async_key", b"async_val")])];
        db.apply_changeset_async(1, &changesets).unwrap();

        // Wait for the write to complete
        db.wait_for_pending_writes();

        // Verify the data was written
        assert_eq!(db.get("bank", 1, b"async_key").unwrap(), Some(b"async_val".to_vec()));

        // Clean up: drop sender to stop worker
        {
            let db_ptr = Arc::as_ptr(&db) as *mut MvccDatabase;
            unsafe {
                (*db_ptr).pending_changes_tx = None;
            }
        }
        handle.join().unwrap();
    }
}
