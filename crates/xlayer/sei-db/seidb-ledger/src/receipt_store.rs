use crate::types::{FilterCriteria, Log, ReceiptRecord, ReceiptStore};
use crossbeam_channel::Sender;
use seidb_common::{
    config::ReceiptStoreConfig,
    error::{Result, SeiDbError},
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_traits::{ss::StateStore, wal::Wal};
use seidb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::{sync::Arc, thread::JoinHandle, time::Duration};
use tracing::{error, info, warn};

/// Store key used for receipt data within the MVCC database.
const RECEIPT_STORE_KEY: &str = "receipt";

/// MVCC-backed receipt store with WAL recovery and background pruning.
///
/// Receipts are indexed by transaction hash and stored in a versioned
/// MVCC database. Each block height maps to a version in the database.
/// A background pruning thread removes old versions to bound disk usage.
pub struct MvccReceiptStore {
    db: Arc<dyn StateStore>,
    #[allow(dead_code)]
    store_key: String,
    stop_pruning: Option<Sender<()>>,
    prune_handle: Option<JoinHandle<()>>,
}

impl MvccReceiptStore {
    /// Create a new MVCC receipt store.
    ///
    /// Opens the underlying state store, replays any WAL entries that were
    /// not yet applied, and starts a background pruning thread if configured.
    pub fn new(
        db: Box<dyn StateStore>,
        config: &ReceiptStoreConfig,
        changelog_path: &str,
    ) -> Result<Self> {
        // Recover from WAL if a changelog path is provided.
        if !changelog_path.is_empty() {
            Self::recover(changelog_path, db.as_ref())?;
        }

        let db: Arc<dyn StateStore> = Arc::from(db);

        // Start background pruning if configured.
        let (stop_pruning, prune_handle) =
            if config.keep_recent > 0 && config.prune_interval_seconds > 0 {
                let (tx, handle) = Self::start_pruning(
                    Arc::clone(&db),
                    config.keep_recent,
                    config.prune_interval_seconds,
                );
                (Some(tx), Some(handle))
            } else {
                (None, None)
            };

        Ok(Self { db, store_key: RECEIPT_STORE_KEY.to_string(), stop_pruning, prune_handle })
    }

    /// Recover receipt state from the WAL.
    ///
    /// Reads the changelog WAL and replays any entries whose version exceeds
    /// the latest version already persisted in the database.
    fn recover(changelog_path: &str, db: &dyn StateStore) -> Result<()> {
        let ss_latest_version = db.get_latest_version();
        info!(changelog_path, ss_latest_version, "Recovering receipt store from changelog");

        let wal_config = seidb_common::config::WalConfig::default();
        let wal: ChangelogWal = new_changelog_wal(wal_config, changelog_path)?;

        let first_offset = match wal.first_offset() {
            Ok(off) if off > 0 => off,
            _ => return Ok(()),
        };
        let last_offset = match wal.last_offset() {
            Ok(off) if off > 0 => off,
            _ => return Ok(()),
        };

        let last_entry = wal.read_at(last_offset)?;

        // Walk backward from the last offset to find the replay start point.
        let mut cur_version = last_entry.version;
        let mut cur_offset = last_offset;
        if ss_latest_version > 0 {
            while cur_version > ss_latest_version && cur_offset > first_offset {
                cur_offset -= 1;
                let entry = wal.read_at(cur_offset)?;
                cur_version = entry.version;
            }
        } else {
            // Fresh store — replay from the beginning.
            cur_offset = first_offset;
        }

        let target_start = cur_offset;
        info!(target_start, last_offset, "Replaying changelog to recover receipt store");

        if target_start < last_offset {
            wal.replay(target_start, last_offset, &mut |_index, entry| {
                db.apply_changeset_sync(entry.version, &entry.changesets)?;
                db.set_latest_version(entry.version)?;
                Ok(())
            })?;
        }

        Ok(())
    }

    /// Spawn a background thread that periodically prunes old receipt versions.
    ///
    /// Returns a channel sender to stop the thread and the thread handle.
    fn start_pruning(
        db: Arc<dyn StateStore>,
        keep_recent: i64,
        interval_secs: i64,
    ) -> (Sender<()>, JoinHandle<()>) {
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

        let handle = std::thread::Builder::new()
            .name("receipt-pruning".to_string())
            .spawn(move || {
                loop {
                    // Check for stop signal before pruning.
                    if stop_rx.try_recv().is_ok() {
                        info!("Receipt store pruning thread stopped");
                        return;
                    }

                    let latest = db.get_latest_version();
                    let prune_version = latest - keep_recent;
                    if prune_version > 0 &&
                        let Err(e) = db.prune(prune_version)
                    {
                        error!(
                            prune_version,
                            error = %e,
                            "Failed to prune receipt store"
                        );
                    }

                    // Sleep with jitter: interval + rand(0..interval).
                    let jitter = (rand::random::<f64>() * interval_secs as f64) as u64;
                    let sleep_secs = interval_secs as u64 + jitter;

                    // Use recv with timeout so we can respond to stop quickly.
                    match stop_rx.recv_timeout(Duration::from_secs(sleep_secs)) {
                        Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            info!("Receipt store pruning thread stopped");
                            return;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            // Continue to next iteration.
                        }
                    }
                }
            })
            .expect("failed to spawn receipt pruning thread");

        (stop_tx, handle)
    }
}

impl ReceiptStore for MvccReceiptStore {
    fn latest_version(&self) -> i64 {
        self.db.get_latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.db.set_latest_version(version)
    }

    fn set_earliest_version(&self, version: i64) -> Result<()> {
        self.db.set_earliest_version(version, false)
    }

    fn get_receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let lv = self.db.get_latest_version();
        self.db.get(RECEIPT_STORE_KEY, lv, tx_hash)
    }

    fn set_receipts(&self, block_height: u64, receipts: &[ReceiptRecord]) -> Result<()> {
        let pairs: Vec<KvPair> = receipts
            .iter()
            .filter(|r| !r.receipt_bytes.is_empty())
            .map(|r| KvPair {
                delete: false,
                key: r.tx_hash.to_vec(),
                value: r.receipt_bytes.clone(),
            })
            .collect();

        let ncs = NamedChangeSet {
            name: RECEIPT_STORE_KEY.to_string(),
            changeset: Some(ChangeSet { pairs }),
        };

        // Map genesis block (height 0) to version 1 to avoid zero-version
        // issues in the underlying MVCC store.
        let version = if block_height == 0 { 1 } else { block_height as i64 };

        self.db.apply_changeset_sync(version, &[ncs])?;
        self.db.set_latest_version(version)?;

        Ok(())
    }

    /// Range-based log filtering is not supported by the MVCC backend since
    /// receipts are indexed by tx hash, not by block number.
    fn filter_logs(&self, _filter: &FilterCriteria) -> Result<Vec<Log>> {
        Err(SeiDbError::Other("range query not supported for MVCC backend".to_string()))
    }

    fn close(&mut self) -> Result<()> {
        // Signal the pruning thread to stop.
        if let Some(tx) = self.stop_pruning.take() {
            let _ = tx.send(());
        }
        // Wait for the pruning thread to finish.
        if let Some(handle) = self.prune_handle.take() &&
            let Err(e) = handle.join()
        {
            warn!("Pruning thread panicked: {:?}", e);
        }
        // Close the underlying database. We need a mutable reference, so
        // use Arc::get_mut which only succeeds when refcount == 1.
        if let Some(db) = Arc::get_mut(&mut self.db) {
            db.close()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::config::StateStoreConfig;
    use seidb_engine::mvcc::db::MvccDatabase;
    use tempfile::TempDir;

    /// Helper: create an MvccDatabase wrapped as Box<dyn StateStore>.
    fn open_test_db(dir: &std::path::Path) -> Box<dyn StateStore> {
        let cfg = StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            use_default_comparer: false,
            ..Default::default()
        };
        Box::new(MvccDatabase::open_db(&cfg).unwrap())
    }

    /// Helper: create a receipt store config with no pruning.
    fn no_prune_config() -> ReceiptStoreConfig {
        ReceiptStoreConfig { keep_recent: 0, prune_interval_seconds: 0, ..Default::default() }
    }

    #[test]
    fn test_mvcc_set_get_receipt() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        let tx_hash = [0xabu8; 32];
        let receipt_bytes = vec![1, 2, 3, 4, 5];
        let records = vec![ReceiptRecord { tx_hash, receipt_bytes: receipt_bytes.clone() }];

        store.set_receipts(10, &records).unwrap();

        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(receipt_bytes));

        // Non-existent hash should return None.
        let missing = [0xcd; 32];
        assert_eq!(store.get_receipt(&missing).unwrap(), None);

        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_receipt_multiple_blocks() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        // Block 1 receipts
        let tx1 = [0x01u8; 32];
        let tx2 = [0x02u8; 32];
        store
            .set_receipts(
                1,
                &[
                    ReceiptRecord { tx_hash: tx1, receipt_bytes: vec![10] },
                    ReceiptRecord { tx_hash: tx2, receipt_bytes: vec![20] },
                ],
            )
            .unwrap();

        // Block 2 receipts
        let tx3 = [0x03u8; 32];
        store.set_receipts(2, &[ReceiptRecord { tx_hash: tx3, receipt_bytes: vec![30] }]).unwrap();

        // All receipts should be retrievable (receipts are immutable, latest version used).
        assert_eq!(store.get_receipt(&tx1).unwrap(), Some(vec![10]));
        assert_eq!(store.get_receipt(&tx2).unwrap(), Some(vec![20]));
        assert_eq!(store.get_receipt(&tx3).unwrap(), Some(vec![30]));

        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_filter_not_supported() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let store = MvccReceiptStore::new(db, &config, "").unwrap();

        let filter = FilterCriteria::default();
        let result = store.filter_logs(&filter);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("range query not supported"));
    }

    #[test]
    fn test_mvcc_genesis_block() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        let tx_hash = [0xff; 32];
        let data = vec![0xde, 0xad];
        store.set_receipts(0, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

        // Genesis block_height=0 should be mapped to version=1 internally.
        assert_eq!(store.latest_version(), 1);
        assert_eq!(store.get_receipt(&tx_hash).unwrap(), Some(data));

        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_close() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        // Write some data.
        let tx_hash = [0x42; 32];
        store.set_receipts(5, &[ReceiptRecord { tx_hash, receipt_bytes: vec![1] }]).unwrap();

        // Close should not panic.
        store.close().unwrap();
        // Second close should also be safe.
        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_version_tracking() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        assert_eq!(store.latest_version(), 0);

        store
            .set_receipts(10, &[ReceiptRecord { tx_hash: [0x01; 32], receipt_bytes: vec![1] }])
            .unwrap();
        assert_eq!(store.latest_version(), 10);

        store
            .set_receipts(20, &[ReceiptRecord { tx_hash: [0x02; 32], receipt_bytes: vec![2] }])
            .unwrap();
        assert_eq!(store.latest_version(), 20);

        // set_latest_version should also work directly.
        store.set_latest_version(50).unwrap();
        assert_eq!(store.latest_version(), 50);

        // set_earliest_version
        store.set_earliest_version(5).unwrap();

        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_pruning() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());

        // Use very short pruning interval for testing.
        let config =
            ReceiptStoreConfig { keep_recent: 2, prune_interval_seconds: 1, ..Default::default() };

        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        // Write receipts across several blocks.
        for height in 1..=10u64 {
            let mut tx_hash = [0u8; 32];
            tx_hash[0] = height as u8;
            store
                .set_receipts(
                    height,
                    &[ReceiptRecord { tx_hash, receipt_bytes: vec![height as u8] }],
                )
                .unwrap();
        }

        // Wait for pruning to run.
        std::thread::sleep(Duration::from_secs(3));

        // Recent receipts should still be available (receipts use latest version for reads).
        let mut tx_hash = [0u8; 32];
        tx_hash[0] = 10;
        assert_eq!(store.get_receipt(&tx_hash).unwrap(), Some(vec![10]));

        store.close().unwrap();
    }

    #[test]
    fn test_mvcc_empty_receipts() {
        let dir = TempDir::new().unwrap();
        let db = open_test_db(dir.path());
        let config = no_prune_config();
        let mut store = MvccReceiptStore::new(db, &config, "").unwrap();

        // Setting empty receipt bytes should be filtered out.
        let tx_hash = [0xaa; 32];
        store.set_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: vec![] }]).unwrap();

        // Should not be found since empty receipts are skipped.
        assert_eq!(store.get_receipt(&tx_hash).unwrap(), None);

        store.close().unwrap();
    }
}
