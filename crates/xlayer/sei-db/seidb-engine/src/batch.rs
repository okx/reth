use rocksdb::{WriteBatch, WriteOptions as RocksWriteOptions};
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::{kv::Batch, types::WriteOptions};
use std::sync::Arc;

/// RocksDB write batch wrapping `rocksdb::WriteBatch`.
///
/// Accumulates put/delete operations in memory and atomically commits them
/// to the underlying RocksDB instance when `commit()` is called.
pub struct RocksDbBatch {
    pub(crate) db: Arc<rocksdb::DB>,
    pub(crate) batch: WriteBatch,
}

impl RocksDbBatch {
    pub fn new(db: Arc<rocksdb::DB>) -> Self {
        Self { db, batch: WriteBatch::default() }
    }
}

/// Convert our `WriteOptions` to RocksDB `WriteOptions`.
fn to_rocks_write_opts(opts: &WriteOptions) -> RocksWriteOptions {
    let mut wo = RocksWriteOptions::default();
    wo.set_sync(opts.sync);
    wo
}

impl Batch for RocksDbBatch {
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.batch.put(key, value);
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.batch.delete(key);
        Ok(())
    }

    fn commit(&mut self, opts: &WriteOptions) -> Result<()> {
        let wo = to_rocks_write_opts(opts);
        self.db
            .write_opt(std::mem::take(&mut self.batch), &wo)
            .map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        Ok(())
    }

    fn len(&self) -> usize {
        self.batch.len()
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    fn reset(&mut self) {
        self.batch.clear();
    }

    fn close(&mut self) -> Result<()> {
        self.batch.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_traits::kv::KvEngine;
    use tempfile::TempDir;

    fn tmp_engine() -> (crate::engine::RocksDbEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = crate::engine::RocksDbEngine::open_plain(dir.path()).unwrap();
        (engine, dir)
    }

    #[test]
    fn test_batch_set_commit() {
        let (engine, _dir) = tmp_engine();
        let mut batch = engine.new_batch();

        batch.set(b"k1", b"v1").unwrap();
        batch.set(b"k2", b"v2").unwrap();
        batch.set(b"k3", b"v3").unwrap();

        batch.commit(&WriteOptions::default()).unwrap();

        assert_eq!(engine.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(engine.get(b"k2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(engine.get(b"k3").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_batch_delete() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();

        // Pre-populate a key
        engine.set(b"del_key", b"del_val", &wo).unwrap();
        assert_eq!(engine.get(b"del_key").unwrap(), Some(b"del_val".to_vec()));

        let mut batch = engine.new_batch();
        batch.set(b"keep_key", b"keep_val").unwrap();
        batch.delete(b"del_key").unwrap();
        batch.commit(&wo).unwrap();

        assert_eq!(engine.get(b"del_key").unwrap(), None);
        assert_eq!(engine.get(b"keep_key").unwrap(), Some(b"keep_val".to_vec()));
    }

    #[test]
    fn test_batch_reset() {
        let (engine, _dir) = tmp_engine();
        let mut batch = engine.new_batch();

        batch.set(b"a", b"1").unwrap();
        batch.set(b"b", b"2").unwrap();
        batch.set(b"c", b"3").unwrap();
        assert_eq!(batch.len(), 3);

        batch.reset();
        assert_eq!(batch.len(), 0);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_batch_len() {
        let (engine, _dir) = tmp_engine();
        let mut batch = engine.new_batch();

        for i in 0..5 {
            batch.set(format!("key{i}").as_bytes(), b"val").unwrap();
        }
        assert_eq!(batch.len(), 5);
        assert!(!batch.is_empty());
    }
}
