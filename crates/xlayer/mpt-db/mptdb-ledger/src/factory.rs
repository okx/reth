use crate::{
    cached_store::CachedReceiptStore, parquet_receipt::ParquetReceiptStore,
    receipt_store::MvccReceiptStore, types::ReceiptStore,
};
use mptdb_common::{
    config::{ParquetStoreConfig, ReceiptStoreConfig, StateStoreConfig},
    error::{MptDbError, Result},
};
use mptdb_engine::mvcc::db::MvccDatabase;

const DEFAULT_ROTATE_INTERVAL: u64 = 500;

/// Create a new receipt store with the configured backend.
///
/// When `backend` is `"parquet"`, a [`ParquetReceiptStore`] backed by binary
/// files is created using `parquet_config`.  Otherwise the default MVCC
/// backend is used.
pub fn new_receipt_store(
    config: &ReceiptStoreConfig,
    ss_changelog_path: &str,
    parquet_config: Option<&ParquetStoreConfig>,
) -> Result<Box<dyn ReceiptStore>> {
    match normalize_backend(&config.backend) {
        "parquet" => {
            let pc = parquet_config
                .ok_or_else(|| MptDbError::Other("parquet config required".into()))?;
            let store = ParquetReceiptStore::new(pc)?;
            let cached = CachedReceiptStore::new(Box::new(store), pc.max_blocks_per_file);
            Ok(Box::new(cached))
        }
        _ => new_mvcc_receipt_store(config, ss_changelog_path),
    }
}

/// Create a new receipt store backed by MVCC with an in-memory cache layer.
///
/// Opens a separate MVCC database instance for receipts (independent of the
/// main state store) and wraps it with a [`CachedReceiptStore`] for fast
/// recent-receipt lookups.
fn new_mvcc_receipt_store(
    config: &ReceiptStoreConfig,
    ss_changelog_path: &str,
) -> Result<Box<dyn ReceiptStore>> {
    let ss_config = StateStoreConfig {
        db_directory: config.db_directory.clone(),
        backend: config.backend.clone(),
        async_write_buffer: config.async_write_buffer,
        keep_recent: config.keep_recent,
        prune_interval_seconds: config.prune_interval_seconds,
        use_default_comparer: config.use_default_comparer,
        ..Default::default()
    };
    let db = MvccDatabase::open_db(&ss_config)?;
    let backend = MvccReceiptStore::new(Box::new(db), config, ss_changelog_path)?;
    let cached = CachedReceiptStore::new(Box::new(backend), DEFAULT_ROTATE_INTERVAL);
    Ok(Box::new(cached))
}

/// Normalize backend name to the canonical form used by the engine layer.
///
/// PebbleDB and empty strings both map to "rocksdb" since the Rust engine
/// uses RocksDB as the underlying implementation for both.
pub fn normalize_backend(backend: &str) -> &str {
    match backend {
        "" | "pebbledb" | "pebble" => "rocksdb",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::config::ReceiptStoreConfig;
    use tempfile::TempDir;

    #[test]
    fn test_factory_creates_store() {
        let dir = TempDir::new().unwrap();
        let config = ReceiptStoreConfig {
            db_directory: dir.path().to_string_lossy().to_string(),
            keep_recent: 0,
            prune_interval_seconds: 0,
            ..Default::default()
        };
        let mut store = new_receipt_store(&config, "", None).unwrap();

        // Verify the store is functional.
        let tx_hash = [0xab; 32];
        let data = vec![1, 2, 3];
        store
            .set_receipts(
                1,
                &[crate::types::ReceiptRecord { tx_hash, receipt_bytes: data.clone() }],
            )
            .unwrap();

        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(data));

        store.close().unwrap();
    }

    #[test]
    fn test_factory_normalize_backend() {
        assert_eq!(normalize_backend(""), "rocksdb");
        assert_eq!(normalize_backend("pebbledb"), "rocksdb");
        assert_eq!(normalize_backend("pebble"), "rocksdb");
        assert_eq!(normalize_backend("rocksdb"), "rocksdb");
        assert_eq!(normalize_backend("sqlite"), "sqlite");
    }
}
