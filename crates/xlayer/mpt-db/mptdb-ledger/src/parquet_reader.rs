use crate::types::{FilterCriteria, Log};
use mptdb_common::error::{MptDbError, Result};
use std::{
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

/// Simple binary file reader for receipts and logs.
///
/// Scans `receipts_{start_block}.bin` and `logs_{start_block}.bin` files
/// in the base directory.  Files are sorted by start block so searches
/// can be narrowed to the relevant block range.
pub struct FileReader {
    base_path: PathBuf,
    max_blocks_per_file: u64,
}

/// Matched pair of receipt + log files for a given start block.
#[derive(Debug, Clone)]
pub struct FilePair {
    pub receipt_file: PathBuf,
    pub log_file: PathBuf,
    pub start_block: u64,
}

impl FileReader {
    pub fn new(base_path: &Path, max_blocks_per_file: u64) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            max_blocks_per_file: if max_blocks_per_file == 0 { 500 } else { max_blocks_per_file },
        }
    }

    /// Retrieve receipt bytes for a given tx hash by scanning all receipt files
    /// (newest first so recent lookups are fast).
    pub fn get_receipt_by_hash(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let mut files = self.scan_receipt_files();
        // Search newest files first for better hit rate on recent receipts.
        files.reverse();
        for path in &files {
            if let Some(data) = self.read_receipt_file(path, tx_hash)? {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    /// Retrieve logs matching a filter by scanning the relevant log files.
    pub fn get_logs(&self, filter: &FilterCriteria) -> Result<Vec<Log>> {
        let files = self.scan_log_files();
        let mut result = Vec::new();
        for path in &files {
            let start_block = extract_block_number(path);
            // Skip files that are entirely outside the requested range.
            if let Some(to) = filter.to_block &&
                start_block > to
            {
                continue;
            }
            if let Some(from) = filter.from_block &&
                start_block + self.max_blocks_per_file <= from
            {
                continue;
            }
            let logs = self.read_log_file(path, filter)?;
            result.extend(logs);
        }
        // Sort by (block_number, tx_index, log_index).
        result.sort_by(|a, b| {
            a.block_number
                .cmp(&b.block_number)
                .then_with(|| a.tx_index.cmp(&b.tx_index))
                .then_with(|| a.log_index.cmp(&b.log_index))
        });
        Ok(result)
    }

    /// Return all `receipts_*.bin` files sorted by start block ascending.
    pub fn scan_receipt_files(&self) -> Vec<PathBuf> {
        self.scan_files_by_prefix("receipts_")
    }

    /// Return all `logs_*.bin` files sorted by start block ascending.
    pub fn scan_log_files(&self) -> Vec<PathBuf> {
        self.scan_files_by_prefix("logs_")
    }

    /// Return file pairs whose data is entirely before `block`.
    pub fn get_files_before_block(&self, block: u64) -> Vec<FilePair> {
        let receipt_files = self.scan_receipt_files();
        let mut pairs = Vec::new();
        for f in receipt_files {
            let start = extract_block_number(&f);
            if start + self.max_blocks_per_file <= block {
                let log_file = self.base_path.join(format!("logs_{start}.bin"));
                pairs.push(FilePair { receipt_file: f, log_file, start_block: start });
            }
        }
        pairs
    }

    /// Find the maximum block number stored across all receipt files.
    /// Returns `(max_block, true)` if found, `(0, false)` otherwise.
    pub fn max_receipt_block_number(&self) -> Result<(u64, bool)> {
        let files = self.scan_receipt_files();
        if files.is_empty() {
            return Ok((0, false));
        }
        // The last file has the highest start block. Scan it to find the
        // actual maximum block number from receipt entries.
        let last = &files[files.len() - 1];
        let start = extract_block_number(last);
        // Receipt files don't store block numbers per entry in our format,
        // so we approximate from filename + max_blocks_per_file.
        // A more precise approach would require scanning, but the filename
        // start_block + max_blocks_per_file - 1 is the upper bound.
        let max_approx = start + self.max_blocks_per_file - 1;
        Ok((max_approx, true))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn scan_files_by_prefix(&self, prefix: &str) -> Vec<PathBuf> {
        let entries = match fs::read_dir(&self.base_path) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("bin") &&
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix))
            })
            .collect();
        files.sort_by_key(|p| extract_block_number(p));
        files
    }

    /// Read a single receipt binary file looking for `tx_hash`.
    ///
    /// File format: repeated `[tx_hash: 32 bytes][data_len: 4 LE][data: N bytes]`.
    fn read_receipt_file(&self, path: &Path, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MptDbError::Io(e)),
        };
        let mut reader = BufReader::new(file);

        loop {
            let mut hash_buf = [0u8; 32];
            match reader.read_exact(&mut hash_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(MptDbError::Io(e)),
            }

            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf)?;
            let data_len = u32::from_le_bytes(len_buf) as usize;

            if hash_buf == *tx_hash {
                let mut data = vec![0u8; data_len];
                reader.read_exact(&mut data)?;
                return Ok(Some(data));
            }

            // Skip data we don't need.
            std::io::copy(&mut reader.by_ref().take(data_len as u64), &mut std::io::sink())?;
        }
        Ok(None)
    }

    /// Read a single log binary file, returning entries that match `filter`.
    ///
    /// See [`write_log`] in `parquet_store` for the encoding.
    fn read_log_file(&self, path: &Path, filter: &FilterCriteria) -> Result<Vec<Log>> {
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(MptDbError::Io(e)),
        };
        let mut reader = BufReader::new(file);
        let mut result = Vec::new();

        loop {
            match read_log_entry(&mut reader) {
                Ok(Some(log)) => {
                    if matches_filter(&log, filter) {
                        result.push(log);
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(result)
    }
}

/// Read a single log entry from a binary stream.
///
/// Returns `Ok(None)` on clean EOF at entry boundary.
fn read_log_entry(reader: &mut impl Read) -> Result<Option<Log>> {
    // address: 20
    let mut address = [0u8; 20];
    match reader.read_exact(&mut address) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(MptDbError::Io(e)),
    }

    // topics_count: 4 LE
    let mut buf4 = [0u8; 4];
    reader.read_exact(&mut buf4)?;
    let topics_count = u32::from_le_bytes(buf4) as usize;

    let mut topics = Vec::with_capacity(topics_count);
    for _ in 0..topics_count {
        let mut t = [0u8; 32];
        reader.read_exact(&mut t)?;
        topics.push(t);
    }

    // data_len: 4 LE + data
    reader.read_exact(&mut buf4)?;
    let data_len = u32::from_le_bytes(buf4) as usize;
    let mut data = vec![0u8; data_len];
    reader.read_exact(&mut data)?;

    // block_number: 8 LE
    let mut buf8 = [0u8; 8];
    reader.read_exact(&mut buf8)?;
    let block_number = u64::from_le_bytes(buf8);

    // tx_hash: 32
    let mut tx_hash = [0u8; 32];
    reader.read_exact(&mut tx_hash)?;

    // tx_index: 4 LE
    reader.read_exact(&mut buf4)?;
    let tx_index = u32::from_le_bytes(buf4);

    // log_index: 4 LE
    reader.read_exact(&mut buf4)?;
    let log_index = u32::from_le_bytes(buf4);

    // block_hash: 32
    let mut block_hash = [0u8; 32];
    reader.read_exact(&mut block_hash)?;

    // removed: 1
    let mut removed_byte = [0u8; 1];
    reader.read_exact(&mut removed_byte)?;
    let removed = removed_byte[0] != 0;

    Ok(Some(Log {
        address,
        topics,
        data,
        block_number,
        tx_hash,
        tx_index,
        log_index,
        block_hash,
        removed,
    }))
}

/// Check whether a log matches the given filter criteria.
fn matches_filter(log: &Log, filter: &FilterCriteria) -> bool {
    if let Some(from) = filter.from_block &&
        log.block_number < from
    {
        return false;
    }
    if let Some(to) = filter.to_block &&
        log.block_number > to
    {
        return false;
    }
    if !filter.addresses.is_empty() && !filter.addresses.contains(&log.address) {
        return false;
    }
    for (i, topic_list) in filter.topics.iter().enumerate() {
        if topic_list.is_empty() {
            continue;
        }
        if i >= log.topics.len() {
            return false;
        }
        if !topic_list.contains(&log.topics[i]) {
            return false;
        }
    }
    true
}

/// Extract the numeric block number from a filename like `receipts_12345.bin`.
pub fn extract_block_number(path: &Path) -> u64 {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // Find the last '_' and parse everything after it.
    if let Some(idx) = stem.rfind('_') {
        stem[idx + 1..].parse::<u64>().unwrap_or(0)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parquet_store::ParquetStore, types::ReceiptRecord};
    use mptdb_common::config::ParquetStoreConfig;
    use tempfile::TempDir;

    fn test_config(dir: &Path) -> ParquetStoreConfig {
        ParquetStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            max_blocks_per_file: 10,
            block_flush_interval: 1,
            ..Default::default()
        }
    }

    #[test]
    fn test_reader_scan_files() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetStore::new(&cfg).unwrap();

        // Write enough blocks to create at least one file.
        for block in 0..5 {
            let mut tx = [0u8; 32];
            tx[0] = block as u8;
            store
                .write_receipts(
                    block,
                    &[ReceiptRecord { tx_hash: tx, receipt_bytes: vec![1] }],
                    &[],
                )
                .unwrap();
        }
        store.flush().unwrap();

        let reader = FileReader::new(dir.path(), 10);
        let rfiles = reader.scan_receipt_files();
        assert!(!rfiles.is_empty(), "should find at least one receipt file");
    }

    #[test]
    fn test_reader_get_receipt() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetStore::new(&cfg).unwrap();

        let tx_hash = [0xab; 32];
        let data = vec![10, 20, 30];
        store
            .write_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }], &[])
            .unwrap();
        store.flush().unwrap();

        let reader = FileReader::new(dir.path(), 10);
        let result = reader.get_receipt_by_hash(&tx_hash).unwrap();
        assert_eq!(result, Some(data));

        // Missing hash.
        let missing = [0xff; 32];
        assert_eq!(reader.get_receipt_by_hash(&missing).unwrap(), None);
    }

    #[test]
    fn test_reader_get_logs_filtered() {
        let dir = TempDir::new().unwrap();
        let cfg = test_config(dir.path());
        let mut store = ParquetStore::new(&cfg).unwrap();

        let addr_a = [0x0a; 20];
        let addr_b = [0x0b; 20];
        let log_a = Log {
            address: addr_a,
            topics: vec![[0x01; 32]],
            data: vec![1],
            block_number: 5,
            tx_hash: [0xaa; 32],
            tx_index: 0,
            log_index: 0,
            block_hash: [0; 32],
            removed: false,
        };
        let log_b = Log {
            address: addr_b,
            topics: vec![[0x02; 32]],
            data: vec![2],
            block_number: 5,
            tx_hash: [0xbb; 32],
            tx_index: 1,
            log_index: 1,
            block_hash: [0; 32],
            removed: false,
        };

        store
            .write_receipts(
                5,
                &[ReceiptRecord { tx_hash: [0xaa; 32], receipt_bytes: vec![1] }],
                &[log_a, log_b],
            )
            .unwrap();
        store.flush().unwrap();

        let reader = FileReader::new(dir.path(), 10);

        // Filter by address.
        let filter = FilterCriteria { addresses: vec![addr_a], ..Default::default() };
        let logs = reader.get_logs(&filter).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, addr_a);

        // Filter by topic.
        let filter = FilterCriteria { topics: vec![vec![[0x02; 32]]], ..Default::default() };
        let logs = reader.get_logs(&filter).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, addr_b);

        // No filter — get all.
        let filter = FilterCriteria::default();
        let logs = reader.get_logs(&filter).unwrap();
        assert_eq!(logs.len(), 2);
    }
}
