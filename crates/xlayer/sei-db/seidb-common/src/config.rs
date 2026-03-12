use crate::error::{Result, SeiDbError};
use std::{str::FromStr, time::Duration};

// ---------------------------------------------------------------------------
// WriteMode
// ---------------------------------------------------------------------------

/// Defines how EVM data writes are routed between backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Writes all data to Cosmos (memiavl) only. Default/legacy behavior.
    #[default]
    CosmosOnly,
    /// Writes EVM data to both Cosmos and EVM backends (migration phase).
    DualWrite,
    /// Writes EVM data to EVM backend and non-EVM data to Cosmos (post-migration).
    SplitWrite,
}

impl WriteMode {
    /// All enum variants are valid by construction.
    pub fn is_valid(&self) -> bool {
        true
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CosmosOnly => "cosmos_only",
            Self::DualWrite => "dual_write",
            Self::SplitWrite => "split_write",
        }
    }
}

impl std::fmt::Display for WriteMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WriteMode {
    type Err = SeiDbError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "cosmos_only" => Ok(Self::CosmosOnly),
            "dual_write" => Ok(Self::DualWrite),
            "split_write" => Ok(Self::SplitWrite),
            _ => Err(SeiDbError::Other(format!("invalid write mode: {s}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// ReadMode
// ---------------------------------------------------------------------------

/// Defines how EVM data reads are routed between backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadMode {
    /// Reads all data from Cosmos (memiavl) only. Default/legacy behavior.
    #[default]
    CosmosOnly,
    /// Reads EVM data from EVM backend first, falls back to Cosmos.
    EvmFirst,
    /// Reads EVM data from EVM backend and non-EVM data from Cosmos.
    SplitRead,
}

impl ReadMode {
    /// All enum variants are valid by construction.
    pub fn is_valid(&self) -> bool {
        true
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CosmosOnly => "cosmos_only",
            Self::EvmFirst => "evm_first",
            Self::SplitRead => "split_read",
        }
    }
}

impl std::fmt::Display for ReadMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReadMode {
    type Err = SeiDbError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "cosmos_only" => Ok(Self::CosmosOnly),
            "evm_first" => Ok(Self::EvmFirst),
            "split_read" => Ok(Self::SplitRead),
            _ => Err(SeiDbError::Other(format!(
                "invalid read mode: \"{s}\", valid modes: cosmos_only, evm_first, split_read"
            ))),
        }
    }
}

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
// MemIavlConfig
// ---------------------------------------------------------------------------

/// Configuration for the MemIAVL (Cosmos) commit store backend.
#[derive(Debug, Clone)]
pub struct MemIavlConfig {
    /// Size of asynchronous commit queue. <= 0 means synchronous commit.
    pub async_commit_buffer: usize,
    /// How many old snapshots (excluding latest) to keep. 0 keeps only current.
    pub snapshot_keep_recent: u32,
    /// Block interval between snapshots. 0 disables auto-snapshots.
    pub snapshot_interval: u32,
    /// Minimum time (seconds) between snapshots to prevent excessive creation.
    pub snapshot_min_time_interval: u32,
    /// Concurrency limit for snapshot writers.
    pub snapshot_writer_limit: usize,
    /// Page cache residency threshold (0.0-1.0) to trigger snapshot prefetch.
    pub snapshot_prefetch_threshold: f64,
    /// Global snapshot write rate limit in MB/s. 0 means unlimited.
    pub snapshot_write_rate_mbps: usize,
}

impl Default for MemIavlConfig {
    fn default() -> Self {
        Self {
            async_commit_buffer: 100,
            snapshot_keep_recent: 0,
            snapshot_interval: 10000,
            snapshot_min_time_interval: 3600,
            snapshot_writer_limit: 4,
            snapshot_prefetch_threshold: 0.8,
            snapshot_write_rate_mbps: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// FlatKvConfig
// ---------------------------------------------------------------------------

/// Configuration for the FlatKV (EVM) commit store backend.
#[derive(Debug, Clone)]
pub struct FlatKvConfig {
    /// Whether PebbleDB writes use fsync. Default: false.
    pub fsync: bool,
    /// Async write buffer size for data DBs. 0 means synchronous.
    pub async_write_buffer: usize,
    /// Block interval between PebbleDB checkpoint snapshots. 0 disables.
    pub snapshot_interval: u32,
    /// Number of old snapshots to keep besides the latest one.
    pub snapshot_keep_recent: u32,
    /// Whether to enable PebbleDB metrics.
    pub enable_db_metrics: bool,
}

impl Default for FlatKvConfig {
    fn default() -> Self {
        Self {
            fsync: false,
            async_write_buffer: 0,
            snapshot_interval: 10000,
            snapshot_keep_recent: 2,
            enable_db_metrics: true,
        }
    }
}

// ---------------------------------------------------------------------------
// StateCommitConfig
// ---------------------------------------------------------------------------

/// Configuration for the state commit (SC) layer.
#[derive(Debug, Clone)]
pub struct StateCommitConfig {
    /// Whether the state-commit (SeiDB) layer is enabled.
    pub enable: bool,
    /// Directory for state-commit store files. Empty uses the application home.
    pub directory: String,
    /// Write routing mode for EVM data.
    pub write_mode: WriteMode,
    /// Read routing mode for EVM data.
    pub read_mode: ReadMode,
    /// MemIAVL (Cosmos) backend configuration.
    pub memiavl: MemIavlConfig,
    /// FlatKV (EVM) backend configuration.
    pub flatkv: FlatKvConfig,
    /// Size of the asynchronous commit queue. <= 0 means synchronous.
    pub async_commit_buffer: usize,
    /// Max concurrent historical proof queries.
    pub historical_proof_max_in_flight: usize,
    /// Token bucket rate (req/sec) for historical proof queries. <= 0 disables.
    pub historical_proof_rate_limit: f64,
    /// Token bucket burst for historical proof queries.
    pub historical_proof_burst: usize,
}

impl Default for StateCommitConfig {
    fn default() -> Self {
        Self {
            enable: true,
            directory: String::new(),
            write_mode: WriteMode::default(),
            read_mode: ReadMode::default(),
            memiavl: MemIavlConfig::default(),
            flatkv: FlatKvConfig::default(),
            async_commit_buffer: 100,
            historical_proof_max_in_flight: 1,
            historical_proof_rate_limit: 1.0,
            historical_proof_burst: 1,
        }
    }
}

impl StateCommitConfig {
    /// Validates the configuration, returning an error if any field is invalid.
    pub fn validate(&self) -> Result<()> {
        if !self.write_mode.is_valid() {
            return Err(SeiDbError::Other(format!("invalid write_mode: {}", self.write_mode)));
        }
        if !self.read_mode.is_valid() {
            return Err(SeiDbError::Other(format!("invalid read_mode: {}", self.read_mode)));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StateStoreConfig
// ---------------------------------------------------------------------------

/// Configuration for the state store (SS) layer.
///
/// EVM optimization is controlled via `write_mode`/`read_mode`:
/// - Both `CosmosOnly` -> EVM stores not opened
/// - Any other mode -> EVM stores opened and used per mode semantics
#[derive(Debug, Clone)]
pub struct StateStoreConfig {
    /// Whether the state-store is enabled for historical queries.
    pub enable: bool,
    /// Directory for state store db files. Empty uses application home.
    pub db_directory: String,
    /// Backend database type. Supported: "pebbledb", "rocksdb".
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
    /// Write routing mode for EVM data.
    pub write_mode: WriteMode,
    /// Read routing mode for EVM data.
    pub read_mode: ReadMode,
    /// Directory for EVM state store db files. Empty uses default location.
    pub evm_db_directory: String,
}

impl Default for StateStoreConfig {
    fn default() -> Self {
        Self {
            enable: true,
            db_directory: String::new(),
            backend: "pebbledb".to_string(),
            async_write_buffer: 100,
            keep_recent: 100_000,
            prune_interval_seconds: 600,
            import_num_workers: 1,
            keep_last_version: true,
            use_default_comparer: false,
            write_mode: WriteMode::default(),
            read_mode: ReadMode::default(),
            evm_db_directory: String::new(),
        }
    }
}

impl StateStoreConfig {
    /// Returns true if EVM state stores should be opened.
    /// Derived from write_mode/read_mode — no separate enable flag needed.
    pub fn evm_enabled(&self) -> bool {
        self.write_mode != WriteMode::CosmosOnly || self.read_mode != ReadMode::CosmosOnly
    }
}

// ---------------------------------------------------------------------------
// ReceiptStoreConfig (Phase 8 placeholder)
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
            backend: "pebbledb".to_string(),
            async_write_buffer: 100,
            keep_recent: 100_000,
            prune_interval_seconds: 600,
            use_default_comparer: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ParquetStoreConfig (Phase 8 placeholder)
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
    fn test_write_mode_parse() {
        assert_eq!("cosmos_only".parse::<WriteMode>().unwrap(), WriteMode::CosmosOnly);
        assert_eq!("dual_write".parse::<WriteMode>().unwrap(), WriteMode::DualWrite);
        assert_eq!("split_write".parse::<WriteMode>().unwrap(), WriteMode::SplitWrite);
    }

    #[test]
    fn test_write_mode_parse_invalid() {
        assert!("invalid".parse::<WriteMode>().is_err());
        assert!("".parse::<WriteMode>().is_err());
        assert!("COSMOS_ONLY".parse::<WriteMode>().is_err());
    }

    #[test]
    fn test_read_mode_parse() {
        assert_eq!("cosmos_only".parse::<ReadMode>().unwrap(), ReadMode::CosmosOnly);
        assert_eq!("evm_first".parse::<ReadMode>().unwrap(), ReadMode::EvmFirst);
        assert_eq!("split_read".parse::<ReadMode>().unwrap(), ReadMode::SplitRead);
    }

    #[test]
    fn test_read_mode_parse_invalid() {
        assert!("invalid".parse::<ReadMode>().is_err());
        assert!("".parse::<ReadMode>().is_err());
        assert!("EVM_FIRST".parse::<ReadMode>().is_err());
    }

    #[test]
    fn test_sc_config_default() {
        let cfg = StateCommitConfig::default();
        assert!(cfg.enable);
        assert!(cfg.directory.is_empty());
        assert_eq!(cfg.write_mode, WriteMode::CosmosOnly);
        assert_eq!(cfg.read_mode, ReadMode::CosmosOnly);
        assert_eq!(cfg.async_commit_buffer, 100);
        assert_eq!(cfg.historical_proof_max_in_flight, 1);
        assert!((cfg.historical_proof_rate_limit - 1.0).abs() < f64::EPSILON);
        assert_eq!(cfg.historical_proof_burst, 1);
        // nested memiavl defaults
        assert_eq!(cfg.memiavl.async_commit_buffer, 100);
        assert_eq!(cfg.memiavl.snapshot_interval, 10000);
        assert_eq!(cfg.memiavl.snapshot_keep_recent, 0);
        assert_eq!(cfg.memiavl.snapshot_min_time_interval, 3600);
        assert_eq!(cfg.memiavl.snapshot_writer_limit, 4);
        assert!((cfg.memiavl.snapshot_prefetch_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.memiavl.snapshot_write_rate_mbps, 100);
        // nested flatkv defaults
        assert!(!cfg.flatkv.fsync);
        assert_eq!(cfg.flatkv.async_write_buffer, 0);
        assert_eq!(cfg.flatkv.snapshot_interval, 10000);
        assert_eq!(cfg.flatkv.snapshot_keep_recent, 2);
        assert!(cfg.flatkv.enable_db_metrics);
        // validate should pass
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_ss_config_default() {
        let cfg = StateStoreConfig::default();
        assert!(cfg.enable);
        assert!(cfg.db_directory.is_empty());
        assert_eq!(cfg.backend, "pebbledb");
        assert_eq!(cfg.async_write_buffer, 100);
        assert_eq!(cfg.keep_recent, 100_000);
        assert_eq!(cfg.prune_interval_seconds, 600);
        assert_eq!(cfg.import_num_workers, 1);
        assert!(cfg.keep_last_version);
        assert!(!cfg.use_default_comparer);
        assert_eq!(cfg.write_mode, WriteMode::CosmosOnly);
        assert_eq!(cfg.read_mode, ReadMode::CosmosOnly);
        assert!(cfg.evm_db_directory.is_empty());
    }

    #[test]
    fn test_ss_config_evm_enabled() {
        // Default: both CosmosOnly -> not enabled
        let cfg = StateStoreConfig::default();
        assert!(!cfg.evm_enabled());

        // DualWrite write mode -> enabled
        let cfg = StateStoreConfig { write_mode: WriteMode::DualWrite, ..Default::default() };
        assert!(cfg.evm_enabled());

        // SplitWrite write mode -> enabled
        let cfg = StateStoreConfig { write_mode: WriteMode::SplitWrite, ..Default::default() };
        assert!(cfg.evm_enabled());

        // EvmFirst read mode -> enabled
        let cfg = StateStoreConfig { read_mode: ReadMode::EvmFirst, ..Default::default() };
        assert!(cfg.evm_enabled());

        // SplitRead read mode -> enabled
        let cfg = StateStoreConfig { read_mode: ReadMode::SplitRead, ..Default::default() };
        assert!(cfg.evm_enabled());

        // Both non-default -> enabled
        let cfg = StateStoreConfig {
            write_mode: WriteMode::SplitWrite,
            read_mode: ReadMode::SplitRead,
            ..Default::default()
        };
        assert!(cfg.evm_enabled());
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
    fn test_write_mode_display() {
        assert_eq!(WriteMode::CosmosOnly.to_string(), "cosmos_only");
        assert_eq!(WriteMode::DualWrite.to_string(), "dual_write");
        assert_eq!(WriteMode::SplitWrite.to_string(), "split_write");
    }

    #[test]
    fn test_read_mode_display() {
        assert_eq!(ReadMode::CosmosOnly.to_string(), "cosmos_only");
        assert_eq!(ReadMode::EvmFirst.to_string(), "evm_first");
        assert_eq!(ReadMode::SplitRead.to_string(), "split_read");
    }

    #[test]
    fn test_receipt_store_config_default() {
        let cfg = ReceiptStoreConfig::default();
        assert!(cfg.db_directory.is_empty());
        assert_eq!(cfg.backend, "pebbledb");
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

    #[test]
    fn test_write_mode_is_valid() {
        assert!(WriteMode::CosmosOnly.is_valid());
        assert!(WriteMode::DualWrite.is_valid());
        assert!(WriteMode::SplitWrite.is_valid());
    }

    #[test]
    fn test_read_mode_is_valid() {
        assert!(ReadMode::CosmosOnly.is_valid());
        assert!(ReadMode::EvmFirst.is_valid());
        assert!(ReadMode::SplitRead.is_valid());
    }
}
