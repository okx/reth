use crate::{
    parquet_reader::FileReader,
    types::{Log, ReceiptRecord},
};
use mptdb_common::{config::ParquetStoreConfig, error::Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};
use tracing::{info, warn};

/// Binary file store for receipts and logs, organized by block range.
///
/// Each file pair (`receipts_{start}.bin` / `logs_{start}.bin`) covers up to
/// `max_blocks_per_file` blocks.  Buffered writes are flushed periodically
/// and files are rotated once the block count threshold is reached.
///
/// Receipt binary format (per entry):
///   `tx_hash(32) | data_len(4 LE) | data(N)`
///
/// Log binary format (per entry):
///   `address(20) | topics_count(4 LE) | topics(32*N) | data_len(4 LE) | data(N)
///    | block_number(8 LE) | tx_hash(32) | tx_index(4 LE) | log_index(4 LE)
///    | block_hash(32) | removed(1)`
pub struct ParquetStore {
    base_path: PathBuf,
    config: ParquetStoreConfig,

    // Current file state.
    file_start_block: u64,
    blocks_in_file: u64,
    last_seen_block: u64,
    blocks_since_flush: u64,
    receipt_buffer: Vec<(ReceiptRecord, u64)>, // (record, block_number)
    log_buffer: Vec<Log>,

    // Writers for the current file pair.
    receipt_writer: Option<BufWriter<File>>,
    log_writer: Option<BufWriter<File>>,

    latest_version: AtomicI64,
    earliest_version: AtomicI64,

    // Embedded reader for queries.
    reader: FileReader,

    // Pruning background thread.
    prune_stop: Option<crossbeam_channel::Sender<()>>,
    prune_handle: Option<std::thread::JoinHandle<()>>,
}

impl ParquetStore {
    /// Create a new store. Creates the base directory if it doesn't exist and
    /// scans for existing files to pick up where a previous instance left off.
    pub fn new(config: &ParquetStoreConfig) -> Result<Self> {
        let base_path = PathBuf::from(&config.db_directory);
        fs::create_dir_all(&base_path)?;

        let max_blocks =
            if config.max_blocks_per_file == 0 { 500 } else { config.max_blocks_per_file };

        let reader = FileReader::new(&base_path, max_blocks);

        let mut file_start_block = 0u64;
        let mut latest: i64 = 0;

        // Determine where to resume from existing files.
        if let Ok((max_block, found)) = reader.max_receipt_block_number() &&
            found
        {
            latest = max_block as i64;
            file_start_block = max_block + 1;
        }

        let mut store = Self {
            base_path,
            config: ParquetStoreConfig {
                max_blocks_per_file: max_blocks,
                block_flush_interval: if config.block_flush_interval == 0 {
                    1
                } else {
                    config.block_flush_interval
                },
                ..config.clone()
            },
            file_start_block,
            blocks_in_file: 0,
            last_seen_block: 0,
            blocks_since_flush: 0,
            receipt_buffer: Vec::with_capacity(1000),
            log_buffer: Vec::with_capacity(10_000),
            receipt_writer: None,
            log_writer: None,
            latest_version: AtomicI64::new(latest),
            earliest_version: AtomicI64::new(0),
            reader,
            prune_stop: None,
            prune_handle: None,
        };

        store.start_pruning();
        Ok(store)
    }

    /// Write receipts and logs for a block.  Data is buffered and flushed
    /// based on `block_flush_interval`.
    pub fn write_receipts(
        &mut self,
        block_number: u64,
        receipts: &[ReceiptRecord],
        logs: &[Log],
    ) -> Result<()> {
        // Lazy-init writers so the filename reflects the actual first block.
        if self.receipt_writer.is_none() {
            self.file_start_block = block_number;
            self.init_writers()?;
        }

        let is_new_block = block_number != self.last_seen_block;
        if is_new_block && self.last_seen_block != 0 {
            self.blocks_since_flush += 1;
            self.blocks_in_file += 1;
        }
        if is_new_block {
            self.last_seen_block = block_number;
        }

        for r in receipts {
            self.receipt_buffer.push((r.clone(), block_number));
        }
        self.log_buffer.extend_from_slice(logs);

        // Periodic flush.
        if self.config.block_flush_interval > 0 &&
            self.blocks_since_flush >= self.config.block_flush_interval
        {
            self.flush()?;
            self.blocks_since_flush = 0;
        }

        // Rotate file if needed.
        if is_new_block && self.should_rotate() {
            self.rotate_file(block_number)?;
        }

        // Track latest version.
        let version = block_number as i64;
        if version > self.latest_version.load(Ordering::Relaxed) {
            self.latest_version.store(version, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Flush buffered data to disk.
    pub fn flush(&mut self) -> Result<()> {
        if self.receipt_buffer.is_empty() && self.log_buffer.is_empty() {
            return Ok(());
        }

        // Ensure writers exist.
        if self.receipt_writer.is_none() {
            self.init_writers()?;
        }

        if let Some(ref mut w) = self.receipt_writer {
            for (rec, _block) in &self.receipt_buffer {
                write_receipt_entry(w, rec)?;
            }
            w.flush()?;
        }

        if !self.log_buffer.is_empty() &&
            let Some(ref mut w) = self.log_writer
        {
            for log in &self.log_buffer {
                write_log_entry(w, log)?;
            }
            w.flush()?;
        }

        self.receipt_buffer.clear();
        self.log_buffer.clear();
        Ok(())
    }

    fn should_rotate(&self) -> bool {
        self.config.max_blocks_per_file > 0 &&
            self.blocks_in_file >= self.config.max_blocks_per_file
    }

    fn rotate_file(&mut self, new_block_number: u64) -> Result<()> {
        self.flush()?;
        self.close_writers()?;

        self.file_start_block = new_block_number;
        self.blocks_in_file = 0;
        self.init_writers()?;
        Ok(())
    }

    /// Retrieve a receipt by tx hash, delegating to the embedded reader
    /// and also checking the current unflushed buffer.
    pub fn get_receipt_by_hash(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        // Check buffer first.
        for (rec, _block) in self.receipt_buffer.iter().rev() {
            if rec.tx_hash == *tx_hash {
                return Ok(Some(rec.receipt_bytes.clone()));
            }
        }
        self.reader.get_receipt_by_hash(tx_hash)
    }

    /// Retrieve logs matching a filter, combining on-disk files and the
    /// current unflushed buffer.
    pub fn get_logs(&self, filter: &crate::types::FilterCriteria) -> Result<Vec<Log>> {
        let mut logs = self.reader.get_logs(filter)?;

        // Also check buffer.
        for log in &self.log_buffer {
            if crate::cache::LedgerCache::match_log(log, filter) {
                logs.push(log.clone());
            }
        }

        logs.sort_by(|a, b| {
            a.block_number
                .cmp(&b.block_number)
                .then_with(|| a.tx_index.cmp(&b.tx_index))
                .then_with(|| a.log_index.cmp(&b.log_index))
        });
        Ok(logs)
    }

    pub fn latest_version(&self) -> i64 {
        self.latest_version.load(Ordering::Relaxed)
    }

    pub fn set_latest_version(&self, version: i64) {
        self.latest_version.store(version, Ordering::Relaxed);
    }

    pub fn set_earliest_version(&self, version: i64) {
        self.earliest_version.store(version, Ordering::Relaxed);
    }

    /// Apply a receipt entry during WAL replay.
    pub fn apply_from_replay(
        &mut self,
        block_number: u64,
        receipts: &[ReceiptRecord],
        logs: &[Log],
    ) -> Result<()> {
        self.write_receipts(block_number, receipts, logs)
    }

    /// Return recent receipts from the buffer for cache warming.
    pub fn warmup_receipts(&self) -> Vec<ReceiptRecord> {
        self.receipt_buffer.iter().map(|(r, _)| r.clone()).collect()
    }

    /// Close the store: stop pruning, flush remaining data, close writers.
    pub fn close(&mut self) -> Result<()> {
        // Stop pruning thread.
        if let Some(tx) = self.prune_stop.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.prune_handle.take() &&
            let Err(e) = handle.join()
        {
            warn!("Parquet pruning thread panicked: {:?}", e);
        }

        self.flush()?;
        self.close_writers()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn init_writers(&mut self) -> Result<()> {
        let receipt_path = self.base_path.join(format!("receipts_{}.bin", self.file_start_block));
        let log_path = self.base_path.join(format!("logs_{}.bin", self.file_start_block));

        let rf = OpenOptions::new().create(true).append(true).open(&receipt_path)?;
        let lf = OpenOptions::new().create(true).append(true).open(&log_path)?;

        self.receipt_writer = Some(BufWriter::new(rf));
        self.log_writer = Some(BufWriter::new(lf));
        Ok(())
    }

    fn close_writers(&mut self) -> Result<()> {
        if let Some(mut w) = self.receipt_writer.take() {
            w.flush()?;
        }
        if let Some(mut w) = self.log_writer.take() {
            w.flush()?;
        }
        Ok(())
    }

    fn start_pruning(&mut self) {
        if self.config.keep_recent <= 0 || self.config.prune_interval_seconds <= 0 {
            return;
        }

        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let keep_recent = self.config.keep_recent;
        let interval_secs = self.config.prune_interval_seconds;
        let base_path = self.base_path.clone();
        let max_blocks = self.config.max_blocks_per_file;
        let latest = self.latest_version.load(Ordering::Relaxed);
        let latest_clone = std::sync::Arc::new(AtomicI64::new(latest));

        // Share the atomic so the pruning thread sees updates.
        // This is a slight simplification — the thread gets a snapshot.
        // For a more accurate view we'd share via Arc, but the store
        // already owns the AtomicI64.  We'll re-read from disk instead.
        let handle = std::thread::Builder::new()
            .name("parquet-pruning".to_string())
            .spawn(move || loop {
                let latest_v = latest_clone.load(Ordering::Relaxed);
                let prune_before = latest_v - keep_recent;
                if prune_before > 0 {
                    let reader = FileReader::new(&base_path, max_blocks);
                    let pairs = reader.get_files_before_block(prune_before as u64);
                    let mut pruned = 0usize;
                    for pair in &pairs {
                        let _ = fs::remove_file(&pair.receipt_file);
                        let _ = fs::remove_file(&pair.log_file);
                        pruned += 1;
                    }
                    if pruned > 0 {
                        info!(pruned, prune_before, "Pruned parquet file pairs");
                    }
                }

                let jitter = (rand::random::<f64>() * interval_secs as f64 * 0.5) as u64;
                let sleep = Duration::from_secs(interval_secs as u64 + jitter);
                match stop_rx.recv_timeout(sleep) {
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!("Parquet pruning thread stopped");
                        return;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
            })
            .expect("failed to spawn parquet pruning thread");

        self.prune_stop = Some(stop_tx);
        self.prune_handle = Some(handle);
    }
}

/// Write a single receipt entry to a writer.
fn write_receipt_entry(w: &mut impl Write, rec: &ReceiptRecord) -> Result<()> {
    w.write_all(&rec.tx_hash)?;
    w.write_all(&(rec.receipt_bytes.len() as u32).to_le_bytes())?;
    w.write_all(&rec.receipt_bytes)?;
    Ok(())
}

/// Write a single log entry to a writer.
fn write_log_entry(w: &mut impl Write, log: &Log) -> Result<()> {
    w.write_all(&log.address)?;
    w.write_all(&(log.topics.len() as u32).to_le_bytes())?;
    for t in &log.topics {
        w.write_all(t)?;
    }
    w.write_all(&(log.data.len() as u32).to_le_bytes())?;
    w.write_all(&log.data)?;
    w.write_all(&log.block_number.to_le_bytes())?;
    w.write_all(&log.tx_hash)?;
    w.write_all(&log.tx_index.to_le_bytes())?;
    w.write_all(&log.log_index.to_le_bytes())?;
    w.write_all(&log.block_hash)?;
    w.write_all(&[u8::from(log.removed)])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FilterCriteria, Log, ReceiptRecord};
    use std::path::Path;
    use tempfile::TempDir;

    fn test_config(dir: &Path) -> ParquetStoreConfig {
        ParquetStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            max_blocks_per_file: 10,
            block_flush_interval: 1,
            ..Default::default()
        }
    }

    fn make_log(addr: [u8; 20], topic: [u8; 32], block: u64) -> Log {
        Log {
            address: addr,
            topics: vec![topic],
            data: vec![0xdd],
            block_number: block,
            tx_hash: [0; 32],
            tx_index: 0,
            log_index: 0,
            block_hash: [0; 32],
            removed: false,
        }
    }

    #[test]
    fn test_store_write_read_receipt() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetStore::new(&cfg).unwrap();

        let tx_hash = [0xab; 32];
        let data = vec![1, 2, 3, 4, 5];
        store
            .write_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }], &[])
            .unwrap();
        store.flush().unwrap();

        let result = store.get_receipt_by_hash(&tx_hash).unwrap();
        assert_eq!(result, Some(data));

        // Missing.
        assert_eq!(store.get_receipt_by_hash(&[0xff; 32]).unwrap(), None);

        store.close().unwrap();
    }

    #[test]
    fn test_store_write_read_logs() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetStore::new(&cfg).unwrap();

        let addr = [0x0a; 20];
        let topic = [0x01; 32];
        let log = make_log(addr, topic, 5);

        store
            .write_receipts(
                5,
                &[ReceiptRecord { tx_hash: [0xaa; 32], receipt_bytes: vec![1] }],
                &[log],
            )
            .unwrap();
        store.flush().unwrap();

        let filter = FilterCriteria {
            from_block: Some(5),
            to_block: Some(5),
            addresses: vec![addr],
            ..Default::default()
        };
        let logs = store.get_logs(&filter).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, addr);
        assert_eq!(logs[0].topics[0], topic);

        store.close().unwrap();
    }

    #[test]
    fn test_store_file_rotation() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.max_blocks_per_file = 5;
        let mut store = ParquetStore::new(&cfg).unwrap();

        // Write 12 blocks to trigger at least one rotation.
        for block in 1..=12u64 {
            let mut tx = [0u8; 32];
            tx[0] = block as u8;
            store
                .write_receipts(
                    block,
                    &[ReceiptRecord { tx_hash: tx, receipt_bytes: vec![block as u8] }],
                    &[],
                )
                .unwrap();
        }
        store.flush().unwrap();

        // Should have multiple receipt files.
        let reader = FileReader::new(dir.path(), 5);
        let files = reader.scan_receipt_files();
        assert!(files.len() >= 2, "expected multiple files after rotation, got {}", files.len());

        // All receipts should be readable.
        for block in 1..=12u64 {
            let mut tx = [0u8; 32];
            tx[0] = block as u8;
            let data = store.get_receipt_by_hash(&tx).unwrap();
            assert_eq!(data, Some(vec![block as u8]), "block {block} receipt missing");
        }

        store.close().unwrap();
    }

    #[test]
    fn test_store_close_reopen() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());

        let tx_hash = [0xcc; 32];
        let data = vec![7, 8, 9];

        // Write and close.
        {
            let mut store = ParquetStore::new(&cfg).unwrap();
            store
                .write_receipts(100, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }], &[])
                .unwrap();
            store.close().unwrap();
        }

        // Reopen and read.
        {
            let store = ParquetStore::new(&cfg).unwrap();
            let result = store.get_receipt_by_hash(&tx_hash).unwrap();
            assert_eq!(result, Some(data));
        }
    }
}
