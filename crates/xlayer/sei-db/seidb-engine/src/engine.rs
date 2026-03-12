use crate::{batch::RocksDbBatch, iterator::RocksDbIterator};
use rocksdb::{
    BlockBasedOptions, Cache, DBCompressionType, Options, WriteOptions as RocksWriteOptions,
};
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::{
    kv::{Batch, Checkpointable, KvEngine, KvIterator},
    types::{IterOptions, WriteOptions},
};
use std::{cmp::Ordering, path::Path, sync::Arc};

/// A custom comparator function: `(name, ordering_fn)`.
pub type ComparatorFn = (String, Box<dyn Fn(&[u8], &[u8]) -> Ordering + Send + Sync>);

/// RocksDB engine wrapper implementing `KvEngine` + `Checkpointable`.
/// Used for both MVCC (32 MB cache) and non-MVCC (512 MB cache) configurations.
pub struct RocksDbEngine {
    db: Arc<rocksdb::DB>,
}

impl RocksDbEngine {
    /// Internal constructor wrapping an already-opened DB.
    fn new(db: rocksdb::DB) -> Self {
        Self { db: Arc::new(db) }
    }

    /// Expose the inner DB handle for the MVCC layer.
    pub fn db(&self) -> &Arc<rocksdb::DB> {
        &self.db
    }

    /// Open a RocksDB instance configured for MVCC state storage.
    ///
    /// Uses a 32 MB block cache, L0 compaction trigger of 2, and optionally
    /// a custom comparator for MVCC key ordering.
    pub fn open_mvcc(data_dir: &Path, comparator_fn: Option<ComparatorFn>) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Zstd);
        opts.set_level_zero_file_num_compaction_trigger(2);

        if let Some((name, cmp_fn)) = comparator_fn {
            opts.set_comparator(&name, Box::new(move |a: &[u8], b: &[u8]| cmp_fn(a, b)));
        }

        let block_opts = Self::configure_block_opts(32);
        opts.set_block_based_table_factory(&block_opts);

        let db =
            rocksdb::DB::open(&opts, data_dir).map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        Ok(Self::new(db))
    }

    /// Open a RocksDB instance configured for plain (non-MVCC) storage.
    ///
    /// Uses a 512 MB block cache and L0 compaction trigger of 4.
    pub fn open_plain(data_dir: &Path) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Zstd);
        opts.set_level_zero_file_num_compaction_trigger(4);

        let block_opts = Self::configure_block_opts(512);
        opts.set_block_based_table_factory(&block_opts);

        let db =
            rocksdb::DB::open(&opts, data_dir).map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        Ok(Self::new(db))
    }

    /// Configure block-based table options shared by both MVCC and plain modes.
    ///
    /// - Block size: 32 KB
    /// - Index (metadata) block size: 256 KB
    /// - Bloom filter: 10 bits per key
    fn configure_block_opts(cache_size_mb: usize) -> BlockBasedOptions {
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_size(32 * 1024); // 32 KB
        block_opts.set_metadata_block_size(256 * 1024); // 256 KB
        block_opts.set_bloom_filter(10.0, false);

        let cache = Cache::new_lru_cache(cache_size_mb * 1024 * 1024);
        block_opts.set_block_cache(&cache);

        block_opts
    }
}

/// Convert our `WriteOptions` to RocksDB `WriteOptions`.
fn to_rocks_write_opts(opts: &WriteOptions) -> RocksWriteOptions {
    let mut wo = RocksWriteOptions::default();
    wo.set_sync(opts.sync);
    wo
}

impl KvEngine for RocksDbEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db
            .get_pinned(key)
            .map(|opt| opt.map(|slice| slice.to_vec()))
            .map_err(|e| SeiDbError::RocksDb(e.to_string()))
    }

    fn set(&self, key: &[u8], value: &[u8], opts: &WriteOptions) -> Result<()> {
        let wo = to_rocks_write_opts(opts);
        self.db.put_opt(key, value, &wo).map_err(|e| SeiDbError::RocksDb(e.to_string()))
    }

    fn delete(&self, key: &[u8], opts: &WriteOptions) -> Result<()> {
        let wo = to_rocks_write_opts(opts);
        self.db.delete_opt(key, &wo).map_err(|e| SeiDbError::RocksDb(e.to_string()))
    }

    fn new_iter(&self, opts: &IterOptions) -> Result<Box<dyn KvIterator>> {
        let iter = RocksDbIterator::new(Arc::clone(&self.db), opts)?;
        Ok(Box::new(iter))
    }

    fn new_batch(&self) -> Box<dyn Batch> {
        Box::new(RocksDbBatch::new(Arc::clone(&self.db)))
    }

    fn flush(&self) -> Result<()> {
        self.db.flush().map_err(|e| SeiDbError::RocksDb(e.to_string()))
    }

    fn close(&mut self) -> Result<()> {
        // No-op: DB is dropped when the last Arc reference is released.
        Ok(())
    }
}

impl Checkpointable for RocksDbEngine {
    fn checkpoint(&self, dest_dir: &Path) -> Result<()> {
        let cp = rocksdb::checkpoint::Checkpoint::new(&*self.db)
            .map_err(|e| SeiDbError::RocksDb(e.to_string()))?;
        cp.create_checkpoint(dest_dir).map_err(|e| SeiDbError::RocksDb(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_engine() -> (RocksDbEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = RocksDbEngine::open_plain(dir.path()).unwrap();
        (engine, dir)
    }

    #[test]
    fn test_open_and_close() {
        let dir = TempDir::new().unwrap();
        let mut engine = RocksDbEngine::open_plain(dir.path()).unwrap();
        // close is a no-op; just ensure it doesn't error
        engine.close().unwrap();
        // engine is dropped here without issue
    }

    #[test]
    fn test_get_set_delete() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();

        engine.set(b"key1", b"val1", &wo).unwrap();
        engine.set(b"key2", b"val2", &wo).unwrap();
        engine.set(b"key3", b"val3", &wo).unwrap();

        assert_eq!(engine.get(b"key1").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(engine.get(b"key2").unwrap(), Some(b"val2".to_vec()));
        assert_eq!(engine.get(b"key3").unwrap(), Some(b"val3".to_vec()));

        engine.delete(b"key2", &wo).unwrap();
        assert_eq!(engine.get(b"key2").unwrap(), None);

        // Other keys remain
        assert_eq!(engine.get(b"key1").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(engine.get(b"key3").unwrap(), Some(b"val3".to_vec()));
    }

    #[test]
    fn test_get_nonexistent() {
        let (engine, _dir) = tmp_engine();
        assert_eq!(engine.get(b"missing_key").unwrap(), None);
    }

    #[test]
    fn test_flush() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"k", b"v", &wo).unwrap();
        engine.flush().unwrap();
    }

    #[test]
    fn test_checkpoint() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"ck_key", b"ck_val", &wo).unwrap();

        let cp_dir = TempDir::new().unwrap();
        let cp_path = cp_dir.path().join("checkpoint");
        engine.checkpoint(&cp_path).unwrap();

        // Open the checkpoint and verify data persisted
        let cp_engine = RocksDbEngine::open_plain(&cp_path).unwrap();
        assert_eq!(cp_engine.get(b"ck_key").unwrap(), Some(b"ck_val".to_vec()));
    }

    #[test]
    fn test_close_idempotent() {
        let dir = TempDir::new().unwrap();
        let mut engine = RocksDbEngine::open_plain(dir.path()).unwrap();
        engine.close().unwrap();
        engine.close().unwrap();
        // Dropping after close is also fine since close is a no-op
    }
}
