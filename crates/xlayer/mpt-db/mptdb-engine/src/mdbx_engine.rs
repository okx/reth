//! MDBX-backed KvEngine implementation.
//!
//! B+ tree storage with copy-on-write transactions, providing significantly
//! better batch write performance than RocksDB's LSM-tree (3-4x measured).

use mptdb_common::error::{MptDbError, Result};
use mptdb_traits::{
    kv::{Batch, KvEngine, KvIterator},
    types::{IterOptions, WriteOptions},
};
use reth_libmdbx::{
    DatabaseFlags, Environment, Geometry, Mode, PageSize, SyncMode, WriteFlags, RO,
};
use std::{
    borrow::Cow,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{mdbx_batch::MdbxBatch, mdbx_iterator::MdbxIterator};

/// Default maximum database size: 256 GB.
const DEFAULT_MAX_SIZE: usize = 256 * 1024 * 1024 * 1024;

/// Default growth step: 4 GB.
const DEFAULT_GROWTH_STEP: isize = 4 * 1024 * 1024 * 1024;

/// MDBX engine implementing the `KvEngine` trait.
///
/// Uses a single unnamed database within the MDBX environment.
/// All operations use the default key ordering (lexicographic).
pub struct MdbxEngine {
    env: Environment,
    closed: AtomicBool,
}

impl MdbxEngine {
    /// Open an MDBX database at the given directory.
    ///
    /// Creates the directory if it doesn't exist. Uses WRITEMAP mode
    /// for better write performance.
    pub fn open(data_dir: &Path) -> Result<Self> {
        Self::open_with_max_size(data_dir, DEFAULT_MAX_SIZE)
    }

    /// Open with a custom maximum database size.
    pub fn open_with_max_size(data_dir: &Path, max_size: usize) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| MptDbError::Other(format!("create dir {}: {e}", data_dir.display())))?;

        let env = Environment::builder()
            .set_max_dbs(1)
            .set_geometry(Geometry {
                size: Some(0..max_size),
                growth_step: Some(DEFAULT_GROWTH_STEP),
                shrink_threshold: Some(0),
                page_size: Some(PageSize::Set(4096)),
            })
            .set_flags(reth_libmdbx::EnvironmentFlags {
                mode: Mode::ReadWrite { sync_mode: SyncMode::SafeNoSync },
                no_rdahead: true,
                coalesce: true,
                ..Default::default()
            })
            .write_map()
            .open(data_dir)
            .map_err(|e| MptDbError::Other(format!("open mdbx: {e}")))?;

        // Ensure the default (unnamed) database exists.
        {
            let txn =
                env.begin_rw_txn().map_err(|e| MptDbError::Other(format!("init txn: {e}")))?;
            txn.create_db(None, DatabaseFlags::default())
                .map_err(|e| MptDbError::Other(format!("create default db: {e}")))?;
            txn.commit().map_err(|e| MptDbError::Other(format!("init commit: {e}")))?;
        }

        Ok(Self { env, closed: AtomicBool::new(false) })
    }

    /// Returns a reference to the underlying MDBX environment.
    pub fn env(&self) -> &Environment {
        &self.env
    }
}

impl KvEngine for MdbxEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.env.begin_ro_txn().map_err(|e| MptDbError::Other(format!("ro txn: {e}")))?;
        let db = txn.open_db(None).map_err(|e| MptDbError::Other(format!("open db: {e}")))?;
        let result: Option<Vec<u8>> =
            txn.get(db.dbi(), key).map_err(|e| MptDbError::Other(format!("get: {e}")))?;
        txn.commit().map_err(|e| MptDbError::Other(format!("ro commit: {e}")))?;
        Ok(result)
    }

    fn set(&self, key: &[u8], value: &[u8], _opts: &WriteOptions) -> Result<()> {
        let txn = self.env.begin_rw_txn().map_err(|e| MptDbError::Other(format!("rw txn: {e}")))?;
        let db = txn.open_db(None).map_err(|e| MptDbError::Other(format!("open db: {e}")))?;
        txn.put(db.dbi(), key, value, WriteFlags::empty())
            .map_err(|e| MptDbError::Other(format!("put: {e}")))?;
        txn.commit().map_err(|e| MptDbError::Other(format!("commit: {e}")))?;
        Ok(())
    }

    fn delete(&self, key: &[u8], _opts: &WriteOptions) -> Result<()> {
        let txn = self.env.begin_rw_txn().map_err(|e| MptDbError::Other(format!("rw txn: {e}")))?;
        let db = txn.open_db(None).map_err(|e| MptDbError::Other(format!("open db: {e}")))?;
        // del returns false if key not found — not an error.
        let _ = txn.del(db.dbi(), key, None).map_err(|e| MptDbError::Other(format!("del: {e}")))?;
        txn.commit().map_err(|e| MptDbError::Other(format!("commit: {e}")))?;
        Ok(())
    }

    fn new_iter(&self, opts: &IterOptions) -> Result<Box<dyn KvIterator>> {
        MdbxIterator::new(&self.env, opts)
    }

    fn new_batch(&self) -> Box<dyn Batch> {
        Box::new(MdbxBatch::new(self.env.clone()))
    }

    fn flush(&self) -> Result<()> {
        // MDBX with SafeNoSync: sync to disk on explicit flush.
        self.env.sync(true).map_err(|e| MptDbError::Other(format!("sync: {e}")))?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.closed.store(true, Ordering::Relaxed);
        // MDBX environment is closed on Drop.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_engine() -> (MdbxEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = MdbxEngine::open(dir.path()).unwrap();
        (engine, dir)
    }

    #[test]
    fn test_open_close() {
        let (mut engine, _dir) = open_engine();
        engine.close().unwrap();
    }

    #[test]
    fn test_get_set() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };

        // Key not found.
        assert!(engine.get(b"missing").unwrap().is_none());

        // Set and get.
        engine.set(b"hello", b"world", &opts).unwrap();
        assert_eq!(engine.get(b"hello").unwrap(), Some(b"world".to_vec()));

        // Update.
        engine.set(b"hello", b"updated", &opts).unwrap();
        assert_eq!(engine.get(b"hello").unwrap(), Some(b"updated".to_vec()));
    }

    #[test]
    fn test_delete() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };

        engine.set(b"key1", b"val1", &opts).unwrap();
        assert!(engine.get(b"key1").unwrap().is_some());

        engine.delete(b"key1", &opts).unwrap();
        assert!(engine.get(b"key1").unwrap().is_none());

        // Delete non-existent key is not an error.
        engine.delete(b"nonexistent", &opts).unwrap();
    }

    #[test]
    fn test_batch() {
        let (engine, _dir) = open_engine();

        let mut batch = engine.new_batch();
        batch.set(b"k1", b"v1").unwrap();
        batch.set(b"k2", b"v2").unwrap();
        batch.set(b"k3", b"v3").unwrap();
        batch.commit(&WriteOptions { sync: false }).unwrap();

        assert_eq!(engine.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(engine.get(b"k2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(engine.get(b"k3").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_batch_with_deletes() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };

        engine.set(b"a", b"1", &opts).unwrap();
        engine.set(b"b", b"2", &opts).unwrap();

        let mut batch = engine.new_batch();
        batch.delete(b"a").unwrap();
        batch.set(b"c", b"3").unwrap();
        batch.commit(&opts).unwrap();

        assert!(engine.get(b"a").unwrap().is_none());
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(engine.get(b"c").unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn test_batch_large() {
        let (engine, _dir) = open_engine();

        // Write 10K key-value pairs in a single batch.
        let mut batch = engine.new_batch();
        for i in 0u32..10_000 {
            let key = format!("key_{:08}", i);
            let val = format!("val_{:08}", i);
            batch.set(key.as_bytes(), val.as_bytes()).unwrap();
        }
        batch.commit(&WriteOptions { sync: false }).unwrap();

        // Verify.
        for i in 0u32..10_000 {
            let key = format!("key_{:08}", i);
            let val = format!("val_{:08}", i);
            assert_eq!(engine.get(key.as_bytes()).unwrap(), Some(val.into_bytes()));
        }
    }

    #[test]
    fn test_iterator_forward() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };

        engine.set(b"a", b"1", &opts).unwrap();
        engine.set(b"b", b"2", &opts).unwrap();
        engine.set(b"c", b"3", &opts).unwrap();

        let mut iter =
            engine.new_iter(&IterOptions { lower_bound: None, upper_bound: None }).unwrap();

        assert!(iter.first());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");

        assert!(iter.next());
        assert_eq!(iter.key(), b"b");

        assert!(iter.next());
        assert_eq!(iter.key(), b"c");

        assert!(!iter.next());
    }

    #[test]
    fn test_iterator_seek() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };

        engine.set(b"aa", b"1", &opts).unwrap();
        engine.set(b"bb", b"2", &opts).unwrap();
        engine.set(b"cc", b"3", &opts).unwrap();

        let mut iter =
            engine.new_iter(&IterOptions { lower_bound: None, upper_bound: None }).unwrap();

        // seek_ge to "b" should land on "bb".
        assert!(iter.seek_ge(b"b"));
        assert_eq!(iter.key(), b"bb");

        // seek_lt to "cc" should land on "bb".
        assert!(iter.seek_lt(b"cc"));
        assert_eq!(iter.key(), b"bb");
    }

    #[test]
    fn test_flush() {
        let (engine, _dir) = open_engine();
        let opts = WriteOptions { sync: false };
        engine.set(b"x", b"y", &opts).unwrap();
        engine.flush().unwrap();
    }
}
