use crate::{batch::RocksDbBatch, iterator::RocksDbIterator};
use mptdb_common::error::{MptDbError, Result};
use mptdb_traits::{
    kv::{Batch, Checkpointable, KvEngine, KvIterator},
    types::{IterOptions, WriteOptions},
};
use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, Cache, DBCompressionType, Options,
    WriteOptions as RocksWriteOptions,
};
use std::{cmp::Ordering, path::Path, sync::Arc};

/// A custom comparator function: `(name, ordering_fn)`.
pub type ComparatorFn = (String, Box<dyn Fn(&[u8], &[u8]) -> Ordering + Send + Sync>);

/// RocksDB engine wrapper implementing `KvEngine` + `Checkpointable`.
/// Used for both MVCC and non-MVCC configurations.
pub struct RocksDbEngine {
    db: Arc<rocksdb::DB>,
}

impl RocksDbEngine {
    const COMPACTION_MEM_BUDGET_BYTES: usize = 512 * 1024 * 1024;
    const BLOCK_SIZE_BYTES: usize = 32 * 1024;
    const METADATA_BLOCK_SIZE_BYTES: usize = 256 * 1024;
    const BLOOM_EQ_BITS_PER_KEY: f64 = 9.9;
    const BLOOM_BEFORE_LEVEL: i32 = 1;
    const MVCC_BLOCK_CACHE_MB: usize = 32;
    const PLAIN_BLOCK_CACHE_MB: usize = 1024;

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
        Self::configure_common_opts(&mut opts);
        opts.set_level_zero_file_num_compaction_trigger(2);

        if let Some((name, cmp_fn)) = comparator_fn {
            opts.set_comparator(&name, Box::new(move |a: &[u8], b: &[u8]| cmp_fn(a, b)));
        }

        let block_opts = Self::configure_block_opts(Self::MVCC_BLOCK_CACHE_MB);
        opts.set_block_based_table_factory(&block_opts);

        let db =
            rocksdb::DB::open(&opts, data_dir).map_err(|e| MptDbError::RocksDb(e.to_string()))?;
        Ok(Self::new(db))
    }

    /// Open a RocksDB instance configured for plain (non-MVCC) storage.
    ///
    /// Uses a 1 GB block cache and L0 compaction trigger of 4.
    pub fn open_plain(data_dir: &Path) -> Result<Self> {
        let mut opts = Options::default();
        Self::configure_common_opts(&mut opts);
        opts.set_level_zero_file_num_compaction_trigger(4);

        let block_opts = Self::configure_block_opts(Self::PLAIN_BLOCK_CACHE_MB);
        opts.set_block_based_table_factory(&block_opts);

        let db =
            rocksdb::DB::open(&opts, data_dir).map_err(|e| MptDbError::RocksDb(e.to_string()))?;
        Ok(Self::new(db))
    }

    /// Configure block-based table options shared by both MVCC and plain modes.
    fn configure_common_opts(opts: &mut Options) {
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Zstd);

        let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get() as i32);
        opts.increase_parallelism(parallelism);
        opts.optimize_level_style_compaction(Self::COMPACTION_MEM_BUDGET_BYTES);
        opts.set_target_file_size_multiplier(2);
        opts.set_level_compaction_dynamic_level_bytes(true);

        // Follow sei-chain style compression tuning for better SST creation throughput.
        opts.set_compression_options_parallel_threads(4);
        opts.set_bottommost_compression_type(DBCompressionType::Zstd);
        opts.set_bottommost_compression_options(0, 12, 0, 112_640, true);
        opts.set_bottommost_zstd_max_train_bytes(11_264_000, true);
    }

    /// - Block size: 32 KB
    /// - Index (metadata) block size: 256 KB
    /// - Hybrid ribbon filter: bloom-equivalent 9.9 bits/key with bloom before level 1
    fn configure_block_opts(cache_size_mb: usize) -> BlockBasedOptions {
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_size(Self::BLOCK_SIZE_BYTES);
        block_opts.set_metadata_block_size(Self::METADATA_BLOCK_SIZE_BYTES);
        block_opts.set_hybrid_ribbon_filter(Self::BLOOM_EQ_BITS_PER_KEY, Self::BLOOM_BEFORE_LEVEL);
        block_opts.set_index_type(BlockBasedIndexType::BinarySearch);
        block_opts.set_optimize_filters_for_memory(true);

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
            .map_err(|e| MptDbError::RocksDb(e.to_string()))
    }

    fn set(&self, key: &[u8], value: &[u8], opts: &WriteOptions) -> Result<()> {
        let wo = to_rocks_write_opts(opts);
        self.db.put_opt(key, value, &wo).map_err(|e| MptDbError::RocksDb(e.to_string()))
    }

    fn delete(&self, key: &[u8], opts: &WriteOptions) -> Result<()> {
        let wo = to_rocks_write_opts(opts);
        self.db.delete_opt(key, &wo).map_err(|e| MptDbError::RocksDb(e.to_string()))
    }

    fn new_iter(&self, opts: &IterOptions) -> Result<Box<dyn KvIterator>> {
        let iter = RocksDbIterator::new(Arc::clone(&self.db), opts)?;
        Ok(Box::new(iter))
    }

    fn new_batch(&self) -> Box<dyn Batch> {
        Box::new(RocksDbBatch::new(Arc::clone(&self.db)))
    }

    fn flush(&self) -> Result<()> {
        self.db.flush().map_err(|e| MptDbError::RocksDb(e.to_string()))
    }

    fn close(&mut self) -> Result<()> {
        // No-op: DB is dropped when the last Arc reference is released.
        Ok(())
    }
}

impl Checkpointable for RocksDbEngine {
    fn checkpoint(&self, dest_dir: &Path) -> Result<()> {
        let cp = rocksdb::checkpoint::Checkpoint::new(&*self.db)
            .map_err(|e| MptDbError::RocksDb(e.to_string()))?;
        cp.create_checkpoint(dest_dir).map_err(|e| MptDbError::RocksDb(e.to_string()))
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
