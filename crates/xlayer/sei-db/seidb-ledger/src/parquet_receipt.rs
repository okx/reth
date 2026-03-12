use crate::{
    parquet_store::ParquetStore,
    types::{FilterCriteria, Log, ReceiptRecord, ReceiptStore},
};
use parking_lot::Mutex;
use seidb_common::{config::ParquetStoreConfig, error::Result};

/// Thread-safe wrapper around [`ParquetStore`] that implements the
/// [`ReceiptStore`] trait.  All operations go through a `Mutex`-protected
/// inner store.
pub struct ParquetReceiptStore {
    store: Mutex<ParquetStore>,
}

impl ParquetReceiptStore {
    pub fn new(config: &ParquetStoreConfig) -> Result<Self> {
        let store = ParquetStore::new(config)?;
        Ok(Self { store: Mutex::new(store) })
    }

    /// Replay WAL entries into the store.  Currently a no-op placeholder
    /// since the binary-file backend doesn't use a separate WAL; data is
    /// written directly to files.  Kept for API parity with the Go version.
    pub fn replay_wal(&self) -> Result<()> {
        Ok(())
    }

    /// Return recently buffered receipts for cache warming.
    pub fn warmup_receipts(&self) -> Vec<ReceiptRecord> {
        self.store.lock().warmup_receipts()
    }
}

impl ReceiptStore for ParquetReceiptStore {
    fn latest_version(&self) -> i64 {
        self.store.lock().latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.store.lock().set_latest_version(version);
        Ok(())
    }

    fn set_earliest_version(&self, version: i64) -> Result<()> {
        self.store.lock().set_earliest_version(version);
        Ok(())
    }

    fn get_receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        self.store.lock().get_receipt_by_hash(tx_hash)
    }

    fn set_receipts(&self, block_height: u64, receipts: &[ReceiptRecord]) -> Result<()> {
        self.store.lock().write_receipts(block_height, receipts, &[])
    }

    fn filter_logs(&self, filter: &FilterCriteria) -> Result<Vec<Log>> {
        self.store.lock().get_logs(filter)
    }

    fn close(&mut self) -> Result<()> {
        self.store.lock().close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FilterCriteria, Log, ReceiptRecord};
    use seidb_common::config::ParquetStoreConfig;
    use tempfile::TempDir;

    fn test_config(dir: &std::path::Path) -> ParquetStoreConfig {
        ParquetStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            max_blocks_per_file: 100,
            block_flush_interval: 1,
            ..Default::default()
        }
    }

    #[test]
    fn test_parquet_receipt_store_basic() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetReceiptStore::new(&cfg).unwrap();

        let tx_hash = [0xab; 32];
        let data = vec![1, 2, 3];
        store.set_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

        // Force flush via direct access.
        store.store.lock().flush().unwrap();

        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(data));

        // Version tracking.
        assert_eq!(store.latest_version(), 1);
        store.set_latest_version(42).unwrap();
        assert_eq!(store.latest_version(), 42);

        store.close().unwrap();
    }

    #[test]
    fn test_parquet_receipt_store_filter_logs() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetReceiptStore::new(&cfg).unwrap();

        // Write receipts, then manually add logs through the inner store.
        {
            let mut inner = store.store.lock();
            let addr = [0x0a; 20];
            let log = Log {
                address: addr,
                topics: vec![[0x01; 32]],
                data: vec![0xff],
                block_number: 10,
                tx_hash: [0xaa; 32],
                tx_index: 0,
                log_index: 0,
                block_hash: [0; 32],
                removed: false,
            };
            inner
                .write_receipts(
                    10,
                    &[ReceiptRecord { tx_hash: [0xaa; 32], receipt_bytes: vec![1] }],
                    &[log],
                )
                .unwrap();
            inner.flush().unwrap();
        }

        let filter = FilterCriteria {
            from_block: Some(10),
            to_block: Some(10),
            addresses: vec![[0x0a; 20]],
            ..Default::default()
        };
        let logs = store.filter_logs(&filter).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, [0x0a; 20]);

        store.close().unwrap();
    }
}
