use crate::error::Result;
use std::time::Duration;

// ---------------------------------------------------------------------------
// WalConfig
// ---------------------------------------------------------------------------

/// Configuration for the write-ahead log.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Number of recent versions to keep. 0 means keep all.
    pub keep_recent: u64,
    /// Auto-prune interval. Duration::ZERO disables auto-pruning.
    pub prune_interval: Duration,
    /// Async write buffer size. 0 means synchronous mode.
    pub write_buffer_size: usize,
    /// Number of entries per write batch.
    pub write_batch_size: usize,
    /// Whether to fsync WAL writes.
    pub fsync_enabled: bool,
    /// Whether to deep-copy data before writing.
    pub deep_copy_enabled: bool,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            keep_recent: 0,
            prune_interval: Duration::ZERO,
            write_buffer_size: 0,
            write_batch_size: 64,
            fsync_enabled: false,
            deep_copy_enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// StateCommitConfig
// ---------------------------------------------------------------------------

/// Configuration for the state commit (SC) layer.
///
/// MPT-only: legacy state-commit backend fields have been removed.
#[derive(Debug, Clone)]
pub struct StateCommitConfig {
    /// Whether the state-commit layer is enabled.
    pub enable: bool,
    /// Directory for state-commit store files. Empty uses the application home.
    pub directory: String,
}

impl Default for StateCommitConfig {
    fn default() -> Self {
        Self { enable: true, directory: String::new() }
    }
}

impl StateCommitConfig {
    /// Validates the configuration, returning an error if any field is invalid.
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StateStoreConfig
// ---------------------------------------------------------------------------

/// Configuration for the state store (SS) layer.
#[derive(Debug, Clone)]
pub struct StateStoreConfig {
    /// Whether the state-store is enabled for historical queries.
    pub enable: bool,
    /// Directory for state store db files. Empty uses application home.
    pub db_directory: String,
    /// Backend database type. Supported: "rocksdb", "mdbx".
    pub backend: String,
    /// Async queue length for commits. <= 0 means synchronous.
    pub async_write_buffer: usize,
    /// Number of versions to keep. 0 means keep everything.
    pub keep_recent: i64,
    /// Interval in seconds between pruning runs.
    pub prune_interval_seconds: i64,
    /// Number of workers used during import.
    pub import_num_workers: usize,
    /// Whether to keep the last version of a key during pruning.
    pub keep_last_version: bool,
    /// Use default lexicographic comparer instead of MVCCComparer.
    /// NOT backwards compatible with existing MVCC databases.
    pub use_default_comparer: bool,
}

impl Default for StateStoreConfig {
    fn default() -> Self {
        Self {
            enable: true,
            db_directory: String::new(),
            backend: "rocksdb".to_string(),
            async_write_buffer: 100,
            keep_recent: 100_000,
            prune_interval_seconds: 600,
            import_num_workers: 1,
            keep_last_version: true,
            use_default_comparer: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ReceiptStoreConfig
// ---------------------------------------------------------------------------

/// Configuration for the receipt store.
#[derive(Debug, Clone)]
pub struct ReceiptStoreConfig {
    /// Directory for receipt store db files.
    pub db_directory: String,
    /// Backend database type.
    pub backend: String,
    /// Async queue length for commits. <= 0 means synchronous.
    pub async_write_buffer: usize,
    /// Number of versions to keep. 0 means keep everything.
    pub keep_recent: i64,
    /// Interval in seconds between pruning runs.
    pub prune_interval_seconds: i64,
    /// Use default lexicographic comparer instead of MVCCComparer.
    pub use_default_comparer: bool,
}

impl Default for ReceiptStoreConfig {
    fn default() -> Self {
        Self {
            db_directory: String::new(),
            backend: "rocksdb".to_string(),
            async_write_buffer: 100,
            keep_recent: 100_000,
            prune_interval_seconds: 600,
            use_default_comparer: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ParquetStoreConfig
// ---------------------------------------------------------------------------

/// Configuration for the parquet store.
#[derive(Debug, Clone)]
pub struct ParquetStoreConfig {
    /// Directory for parquet store files.
    pub db_directory: String,
    /// Number of versions to keep. 0 means keep everything.
    pub keep_recent: i64,
    /// Interval in seconds between pruning runs.
    pub prune_interval_seconds: i64,
    /// Block flush interval.
    pub block_flush_interval: u64,
    /// Maximum number of blocks per parquet file.
    pub max_blocks_per_file: u64,
}

impl Default for ParquetStoreConfig {
    fn default() -> Self {
        Self {
            db_directory: String::new(),
            keep_recent: 0,
            prune_interval_seconds: 600,
            block_flush_interval: 1,
            max_blocks_per_file: 500,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sc_config_default() {
        let cfg = StateCommitConfig::default();
        assert!(cfg.enable);
        assert!(cfg.directory.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_ss_config_default() {
        let cfg = StateStoreConfig::default();
        assert!(cfg.enable);
        assert!(cfg.db_directory.is_empty());
        assert_eq!(cfg.backend, "rocksdb");
        assert_eq!(cfg.async_write_buffer, 100);
        assert_eq!(cfg.keep_recent, 100_000);
        assert_eq!(cfg.prune_interval_seconds, 600);
        assert_eq!(cfg.import_num_workers, 1);
        assert!(cfg.keep_last_version);
        assert!(!cfg.use_default_comparer);
    }

    #[test]
    fn test_wal_config_default() {
        let cfg = WalConfig::default();
        assert_eq!(cfg.keep_recent, 0);
        assert_eq!(cfg.prune_interval, Duration::ZERO);
        assert_eq!(cfg.write_buffer_size, 0);
        assert_eq!(cfg.write_batch_size, 64);
        assert!(!cfg.fsync_enabled);
        assert!(!cfg.deep_copy_enabled);
    }

    #[test]
    fn test_receipt_store_config_default() {
        let cfg = ReceiptStoreConfig::default();
        assert!(cfg.db_directory.is_empty());
        assert_eq!(cfg.backend, "rocksdb");
        assert_eq!(cfg.async_write_buffer, 100);
        assert_eq!(cfg.keep_recent, 100_000);
        assert_eq!(cfg.prune_interval_seconds, 600);
        assert!(!cfg.use_default_comparer);
    }

    #[test]
    fn test_parquet_store_config_default() {
        let cfg = ParquetStoreConfig::default();
        assert!(cfg.db_directory.is_empty());
        assert_eq!(cfg.keep_recent, 0);
        assert_eq!(cfg.prune_interval_seconds, 600);
        assert_eq!(cfg.block_flush_interval, 1);
        assert_eq!(cfg.max_blocks_per_file, 500);
    }
}
