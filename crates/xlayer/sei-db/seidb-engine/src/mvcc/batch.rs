use crate::mvcc::{
    constants::{LATEST_VERSION_KEY, TOMBSTONE_VAL},
    db::prepend_store_key,
    encoding::{mvcc_encode, mvcc_encode_value},
};
use rocksdb::{WriteBatch, DB};
use seidb_common::error::{Result, SeiDbError};
use std::sync::Arc;

/// Fixed-version MVCC batch. All keys are encoded at the same version.
///
/// On creation the batch immediately records `LATEST_VERSION_KEY` so that
/// committing the batch atomically advances the database's version counter.
#[allow(dead_code)]
pub(crate) struct MvccBatch {
    db: Arc<DB>,
    batch: WriteBatch,
    version: i64,
}

#[allow(dead_code)]
impl MvccBatch {
    /// Create a new batch pinned to `version`.
    ///
    /// The latest-version metadata key is written into the batch immediately
    /// so that a single `write()` call atomically bumps the version.
    pub(crate) fn new(db: Arc<DB>, version: i64) -> Result<Self> {
        let mut batch = WriteBatch::default();
        batch.put(LATEST_VERSION_KEY, (version as u64).to_le_bytes());
        Ok(Self { db, batch, version })
    }

    /// Encode and insert a key/value pair with an optional tombstone marker.
    fn set_internal(
        &mut self,
        store_key: &str,
        tombstone: i64,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let prefixed_key = mvcc_encode(&prepend_store_key(store_key, key), self.version);
        let prefixed_val = mvcc_encode_value(value, tombstone);
        self.batch.put(&prefixed_key, &prefixed_val);
        Ok(())
    }

    /// Set a key/value pair (no tombstone).
    pub(crate) fn set(&mut self, store_key: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.set_internal(store_key, 0, key, value)
    }

    /// Logically delete a key by writing a tombstone marker at this batch's
    /// version.
    pub(crate) fn delete(&mut self, store_key: &str, key: &[u8]) -> Result<()> {
        self.set_internal(store_key, self.version, key, TOMBSTONE_VAL)
    }

    /// Physically remove the MVCC-encoded key from the database.
    pub(crate) fn hard_delete(&mut self, store_key: &str, key: &[u8]) -> Result<()> {
        let full_key = mvcc_encode(&prepend_store_key(store_key, key), self.version);
        self.batch.delete(&full_key);
        Ok(())
    }

    /// Atomically commit all pending operations to RocksDB and reset the
    /// internal batch.
    pub(crate) fn write(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        self.db.write(batch).map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        self.batch = WriteBatch::default();
        Ok(())
    }

    /// Number of operations currently buffered in the batch.
    pub(crate) fn size(&self) -> usize {
        self.batch.len()
    }

    /// Discard all buffered operations.
    pub(crate) fn reset(&mut self) {
        self.batch.clear();
    }
}

/// Variable-version batch for import/recovery scenarios where each key may
/// carry a different version.
#[allow(dead_code)]
pub(crate) struct MvccRawBatch {
    db: Arc<DB>,
    batch: WriteBatch,
}

#[allow(dead_code)]
impl MvccRawBatch {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self { db, batch: WriteBatch::default() }
    }

    /// Set a key/value pair at an explicit `version`.
    pub(crate) fn set(
        &mut self,
        store_key: &str,
        key: &[u8],
        value: &[u8],
        version: i64,
    ) -> Result<()> {
        let prefixed_key = mvcc_encode(&prepend_store_key(store_key, key), version);
        let prefixed_val = mvcc_encode_value(value, 0);
        self.batch.put(&prefixed_key, &prefixed_val);
        Ok(())
    }

    /// Logically delete a key at an explicit `version` by writing a tombstone.
    pub(crate) fn delete(&mut self, store_key: &str, key: &[u8], version: i64) -> Result<()> {
        let prefixed_key = mvcc_encode(&prepend_store_key(store_key, key), version);
        let prefixed_val = mvcc_encode_value(TOMBSTONE_VAL, version);
        self.batch.put(&prefixed_key, &prefixed_val);
        Ok(())
    }

    /// Atomically commit all pending operations to RocksDB and reset the
    /// internal batch.
    pub(crate) fn write(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        self.db.write(batch).map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        self.batch = WriteBatch::default();
        Ok(())
    }

    /// Number of operations currently buffered in the batch.
    pub(crate) fn size(&self) -> usize {
        self.batch.len()
    }

    /// Discard all buffered operations.
    pub(crate) fn reset(&mut self) {
        self.batch.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::db::MvccDatabase;
    use seidb_common::config::StateStoreConfig;
    use tempfile::TempDir;

    /// Helper: build a minimal StateStoreConfig pointing at the given dir.
    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            use_default_comparer: false,
            ..Default::default()
        }
    }

    #[test]
    fn test_batch_set_and_write() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut batch = MvccBatch::new(Arc::clone(mvcc.db()), 1).unwrap();
        batch.set("store", b"k1", b"v1").unwrap();
        batch.set("store", b"k2", b"v2").unwrap();
        batch.set("store", b"k3", b"v3").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get("store", 1, b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(mvcc.get("store", 1, b"k2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(mvcc.get("store", 1, b"k3").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_batch_delete_tombstone() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 1.
        let mut batch = MvccBatch::new(Arc::clone(mvcc.db()), 1).unwrap();
        batch.set("store", b"key", b"alive").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"alive".to_vec()));

        // Delete at version 2.
        let mut batch2 = MvccBatch::new(Arc::clone(mvcc.db()), 2).unwrap();
        batch2.delete("store", b"key").unwrap();
        batch2.write().unwrap();

        // At version 2 the key is tombstoned.
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
        // At version 1 the key was still alive.
        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"alive".to_vec()));
    }

    #[test]
    fn test_batch_hard_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 5.
        let mut batch = MvccBatch::new(Arc::clone(mvcc.db()), 5).unwrap();
        batch.set("store", b"key", b"value").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get("store", 5, b"key").unwrap(), Some(b"value".to_vec()));

        // Hard delete at version 5.
        let mut batch2 = MvccBatch::new(Arc::clone(mvcc.db()), 5).unwrap();
        batch2.hard_delete("store", b"key").unwrap();
        batch2.write().unwrap();

        // The raw entry is physically gone.
        assert_eq!(mvcc.get("store", 5, b"key").unwrap(), None);
    }

    #[test]
    fn test_batch_size_and_reset() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut batch = MvccBatch::new(Arc::clone(mvcc.db()), 1).unwrap();
        // The batch already contains the LATEST_VERSION_KEY entry from new().
        let initial = batch.size();

        batch.set("store", b"a", b"1").unwrap();
        batch.set("store", b"b", b"2").unwrap();
        batch.set("store", b"c", b"3").unwrap();
        assert!(batch.size() > initial);

        batch.reset();
        assert_eq!(batch.size(), 0);
    }

    #[test]
    fn test_raw_batch_multi_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut raw = MvccRawBatch::new(Arc::clone(mvcc.db()));
        raw.set("store", b"key", b"v1", 1).unwrap();
        raw.set("store", b"key", b"v2", 2).unwrap();
        raw.set("store", b"key", b"v3", 3).unwrap();
        raw.write().unwrap();

        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(mvcc.get("store", 3, b"key").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_raw_batch_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut raw = MvccRawBatch::new(Arc::clone(mvcc.db()));
        raw.set("store", b"key", b"alive", 1).unwrap();
        raw.delete("store", b"key", 2).unwrap();
        raw.write().unwrap();

        // At version 1 value is alive.
        assert_eq!(mvcc.get("store", 1, b"key").unwrap(), Some(b"alive".to_vec()));
        // At version 2 tombstone applies.
        assert_eq!(mvcc.get("store", 2, b"key").unwrap(), None);
    }
}
