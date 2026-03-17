use crate::mvcc::{
    constants::{LATEST_VERSION_KEY, TOMBSTONE_VAL},
    encoding::{mvcc_encode, mvcc_encode_value},
};
use mptdb_common::error::Result;
use mptdb_traits::{
    kv::{Batch, KvEngine},
    types::WriteOptions,
};

/// Fixed-version MVCC batch. All keys are encoded at the same version.
///
/// On creation the batch immediately records `LATEST_VERSION_KEY` so that
/// committing the batch atomically advances the database's version counter.
#[allow(dead_code)]
pub(crate) struct MvccBatch {
    batch: Box<dyn Batch>,
    version: i64,
}

#[allow(dead_code)]
impl MvccBatch {
    /// Create a new batch pinned to `version`.
    ///
    /// The latest-version metadata key is written into the batch immediately
    /// so that a single `commit()` call atomically bumps the version.
    pub(crate) fn new(engine: &dyn KvEngine, version: i64) -> Result<Self> {
        let mut batch = engine.new_batch();
        batch.set(LATEST_VERSION_KEY, &(version as u64).to_le_bytes())?;
        Ok(Self { batch, version })
    }

    /// Encode and insert a key/value pair with an optional tombstone marker.
    fn set_internal(
        &mut self,
        tombstone: i64,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let encoded_key = mvcc_encode(key, self.version);
        let encoded_val = mvcc_encode_value(value, tombstone);
        self.batch.set(&encoded_key, &encoded_val)?;
        Ok(())
    }

    /// Set a key/value pair (no tombstone).
    pub(crate) fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.set_internal(0, key, value)
    }

    /// Logically delete a key by writing a tombstone marker at this batch's
    /// version.
    pub(crate) fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.set_internal(self.version, key, TOMBSTONE_VAL)
    }

    /// Physically remove the MVCC-encoded key from the database.
    pub(crate) fn hard_delete(&mut self, key: &[u8]) -> Result<()> {
        let full_key = mvcc_encode(key, self.version);
        self.batch.delete(&full_key)?;
        Ok(())
    }

    /// Atomically commit all pending operations and reset the internal batch.
    pub(crate) fn write(&mut self) -> Result<()> {
        self.batch.commit(&WriteOptions::default())?;
        self.batch.reset();
        Ok(())
    }

    /// Number of operations currently buffered in the batch.
    pub(crate) fn size(&self) -> usize {
        self.batch.len()
    }

    /// Discard all buffered operations.
    pub(crate) fn reset(&mut self) {
        self.batch.reset();
    }
}

/// Variable-version batch for import/recovery scenarios where each key may
/// carry a different version.
#[allow(dead_code)]
pub(crate) struct MvccRawBatch {
    batch: Box<dyn Batch>,
}

#[allow(dead_code)]
impl MvccRawBatch {
    pub(crate) fn new(engine: &dyn KvEngine) -> Self {
        Self { batch: engine.new_batch() }
    }

    /// Set a key/value pair at an explicit `version`.
    pub(crate) fn set(
        &mut self,
        key: &[u8],
        value: &[u8],
        version: i64,
    ) -> Result<()> {
        let encoded_key = mvcc_encode(key, version);
        let encoded_val = mvcc_encode_value(value, 0);
        self.batch.set(&encoded_key, &encoded_val)?;
        Ok(())
    }

    /// Logically delete a key at an explicit `version` by writing a tombstone.
    pub(crate) fn delete(&mut self, key: &[u8], version: i64) -> Result<()> {
        let encoded_key = mvcc_encode(key, version);
        let encoded_val = mvcc_encode_value(TOMBSTONE_VAL, version);
        self.batch.set(&encoded_key, &encoded_val)?;
        Ok(())
    }

    /// Atomically commit all pending operations and reset the internal batch.
    pub(crate) fn write(&mut self) -> Result<()> {
        self.batch.commit(&WriteOptions::default())?;
        self.batch.reset();
        Ok(())
    }

    /// Number of operations currently buffered in the batch.
    pub(crate) fn size(&self) -> usize {
        self.batch.len()
    }

    /// Discard all buffered operations.
    pub(crate) fn reset(&mut self) {
        self.batch.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvcc::db::MvccDatabase;
    use mptdb_common::config::StateStoreConfig;
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

        let mut batch = MvccBatch::new(mvcc.engine.as_ref(), 1).unwrap();
        batch.set(b"k1", b"v1").unwrap();
        batch.set(b"k2", b"v2").unwrap();
        batch.set(b"k3", b"v3").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get(1, b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(mvcc.get(1, b"k2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(mvcc.get(1, b"k3").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_batch_delete_tombstone() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 1.
        let mut batch = MvccBatch::new(mvcc.engine.as_ref(), 1).unwrap();
        batch.set(b"key", b"alive").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"alive".to_vec()));

        // Delete at version 2.
        let mut batch2 = MvccBatch::new(mvcc.engine.as_ref(), 2).unwrap();
        batch2.delete(b"key").unwrap();
        batch2.write().unwrap();

        // At version 2 the key is tombstoned.
        assert_eq!(mvcc.get(2, b"key").unwrap(), None);
        // At version 1 the key was still alive.
        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"alive".to_vec()));
    }

    #[test]
    fn test_batch_hard_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        // Write value at version 5.
        let mut batch = MvccBatch::new(mvcc.engine.as_ref(), 5).unwrap();
        batch.set(b"key", b"value").unwrap();
        batch.write().unwrap();

        assert_eq!(mvcc.get(5, b"key").unwrap(), Some(b"value".to_vec()));

        // Hard delete at version 5.
        let mut batch2 = MvccBatch::new(mvcc.engine.as_ref(), 5).unwrap();
        batch2.hard_delete(b"key").unwrap();
        batch2.write().unwrap();

        // The raw entry is physically gone.
        assert_eq!(mvcc.get(5, b"key").unwrap(), None);
    }

    #[test]
    fn test_batch_size_and_reset() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut batch = MvccBatch::new(mvcc.engine.as_ref(), 1).unwrap();
        // The batch already contains the LATEST_VERSION_KEY entry from new().
        let initial = batch.size();

        batch.set(b"a", b"1").unwrap();
        batch.set(b"b", b"2").unwrap();
        batch.set(b"c", b"3").unwrap();
        assert!(batch.size() > initial);

        batch.reset();
        assert_eq!(batch.size(), 0);
    }

    #[test]
    fn test_raw_batch_multi_version() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut raw = MvccRawBatch::new(mvcc.engine.as_ref());
        raw.set(b"key", b"v1", 1).unwrap();
        raw.set(b"key", b"v2", 2).unwrap();
        raw.set(b"key", b"v3", 3).unwrap();
        raw.write().unwrap();

        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(mvcc.get(2, b"key").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(mvcc.get(3, b"key").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_raw_batch_delete() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mvcc = MvccDatabase::open_db(&cfg).unwrap();

        let mut raw = MvccRawBatch::new(mvcc.engine.as_ref());
        raw.set(b"key", b"alive", 1).unwrap();
        raw.delete(b"key", 2).unwrap();
        raw.write().unwrap();

        // At version 1 value is alive.
        assert_eq!(mvcc.get(1, b"key").unwrap(), Some(b"alive".to_vec()));
        // At version 2 tombstone applies.
        assert_eq!(mvcc.get(2, b"key").unwrap(), None);
    }
}
