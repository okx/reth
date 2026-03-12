use crate::{
    cache::LedgerCache,
    types::{FilterCriteria, Log, ReceiptRecord, ReceiptStore},
};
use parking_lot::Mutex;
use seidb_common::error::Result;
use std::collections::HashSet;

/// Receipt store wrapper that adds an in-memory cache layer in front of a
/// persistent backend. Recent receipts and logs are served from the cache,
/// falling back to the backend on cache miss.
///
/// The cache rotates periodically (every `rotate_interval` blocks) to bound
/// memory usage while keeping a sliding window of recent data.
pub struct CachedReceiptStore {
    backend: Box<dyn ReceiptStore>,
    cache: LedgerCache,
    rotate_interval: u64,
    next_rotate_block: Mutex<u64>,
}

impl CachedReceiptStore {
    /// Create a new cached receipt store wrapping the given backend.
    ///
    /// `rotate_interval` controls how many blocks worth of data the cache
    /// holds before rotating out the oldest chunk. A value of 0 disables
    /// rotation entirely.
    pub fn new(backend: Box<dyn ReceiptStore>, rotate_interval: u64) -> Self {
        Self {
            backend,
            cache: LedgerCache::new(),
            rotate_interval,
            next_rotate_block: Mutex::new(0),
        }
    }

    /// Rotate the cache if `block_number` has reached the next rotation threshold.
    fn maybe_rotate(&self, block_number: u64) {
        if self.rotate_interval == 0 {
            return;
        }
        let mut next = self.next_rotate_block.lock();
        if *next == 0 {
            *next = block_number + self.rotate_interval;
            return;
        }
        while block_number >= *next {
            self.cache.rotate();
            *next += self.rotate_interval;
        }
    }
}

impl ReceiptStore for CachedReceiptStore {
    fn latest_version(&self) -> i64 {
        self.backend.latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.backend.set_latest_version(version)
    }

    fn set_earliest_version(&self, version: i64) -> Result<()> {
        self.backend.set_earliest_version(version)
    }

    fn get_receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        // Try cache first.
        if let Some(data) = self.cache.get_receipt(tx_hash) {
            return Ok(Some(data));
        }
        // Fall back to persistent backend.
        self.backend.get_receipt(tx_hash)
    }

    fn set_receipts(&self, block_height: u64, receipts: &[ReceiptRecord]) -> Result<()> {
        // Persist to backend first.
        self.backend.set_receipts(block_height, receipts)?;
        // Populate cache.
        self.cache.add_receipts_batch(block_height, receipts);
        // Rotate if we've crossed the threshold.
        self.maybe_rotate(block_height);
        Ok(())
    }

    fn filter_logs(&self, filter: &FilterCriteria) -> Result<Vec<Log>> {
        let cache_logs = self.cache.filter_logs(filter);
        let backend_logs = self.backend.filter_logs(filter).unwrap_or_default();

        if cache_logs.is_empty() {
            return Ok(backend_logs);
        }
        if backend_logs.is_empty() {
            let mut logs = cache_logs;
            sort_logs(&mut logs);
            return Ok(logs);
        }

        // Deduplicate by (block_number, tx_index, log_index).
        let mut seen: HashSet<(u64, u32, u32)> = HashSet::with_capacity(backend_logs.len());
        for lg in &backend_logs {
            seen.insert((lg.block_number, lg.tx_index, lg.log_index));
        }

        let mut result = backend_logs;
        for lg in cache_logs {
            let key = (lg.block_number, lg.tx_index, lg.log_index);
            if !seen.contains(&key) {
                result.push(lg);
            }
        }

        sort_logs(&mut result);
        Ok(result)
    }

    fn close(&mut self) -> Result<()> {
        self.backend.close()
    }
}

fn sort_logs(logs: &mut [Log]) {
    logs.sort_by(|a, b| {
        a.block_number
            .cmp(&b.block_number)
            .then_with(|| a.tx_index.cmp(&b.tx_index))
            .then_with(|| a.log_index.cmp(&b.log_index))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FilterCriteria, Log, ReceiptRecord, ReceiptStore};
    use seidb_common::error::{Result, SeiDbError};
    use std::sync::{
        atomic::{AtomicI64, Ordering},
        Mutex as StdMutex,
    };

    /// Simple in-memory backend for testing the cache layer.
    struct MockBackend {
        receipts: StdMutex<std::collections::HashMap<[u8; 32], Vec<u8>>>,
        latest: AtomicI64,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                receipts: StdMutex::new(std::collections::HashMap::new()),
                latest: AtomicI64::new(0),
            }
        }
    }

    impl ReceiptStore for MockBackend {
        fn latest_version(&self) -> i64 {
            self.latest.load(Ordering::SeqCst)
        }
        fn set_latest_version(&self, version: i64) -> Result<()> {
            self.latest.store(version, Ordering::SeqCst);
            Ok(())
        }
        fn set_earliest_version(&self, _version: i64) -> Result<()> {
            Ok(())
        }
        fn get_receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
            Ok(self.receipts.lock().unwrap().get(tx_hash).cloned())
        }
        fn set_receipts(&self, block_height: u64, receipts: &[ReceiptRecord]) -> Result<()> {
            let mut map = self.receipts.lock().unwrap();
            for r in receipts {
                if !r.receipt_bytes.is_empty() {
                    map.insert(r.tx_hash, r.receipt_bytes.clone());
                }
            }
            self.latest.store(block_height as i64, Ordering::SeqCst);
            Ok(())
        }
        fn filter_logs(&self, _filter: &FilterCriteria) -> Result<Vec<Log>> {
            Err(SeiDbError::Other("not supported".to_string()))
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_cached_store_cache_hit() {
        let mut store = CachedReceiptStore::new(Box::new(MockBackend::new()), 500);
        let tx_hash = [0xaa; 32];
        let data = vec![1, 2, 3];
        store.set_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

        // Should come from cache (no backend round-trip needed for verification).
        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(data));

        store.close().unwrap();
    }

    #[test]
    fn test_cached_store_cache_miss() {
        // Pre-populate backend directly, bypassing cache.
        let backend = MockBackend::new();
        backend.receipts.lock().unwrap().insert([0xbb; 32], vec![10, 20]);

        let store = CachedReceiptStore::new(Box::new(backend), 500);

        // Not in cache, should fall back to backend.
        let result = store.get_receipt(&[0xbb; 32]).unwrap();
        assert_eq!(result, Some(vec![10, 20]));

        // Completely unknown hash.
        let result = store.get_receipt(&[0xff; 32]).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_cached_store_set_populates_cache() {
        let mut store = CachedReceiptStore::new(Box::new(MockBackend::new()), 500);

        let tx1 = [0x01; 32];
        let tx2 = [0x02; 32];
        store
            .set_receipts(
                10,
                &[
                    ReceiptRecord { tx_hash: tx1, receipt_bytes: vec![1] },
                    ReceiptRecord { tx_hash: tx2, receipt_bytes: vec![2] },
                ],
            )
            .unwrap();

        // Both should be in cache.
        assert_eq!(store.cache.get_receipt(&tx1), Some(vec![1]));
        assert_eq!(store.cache.get_receipt(&tx2), Some(vec![2]));

        // And also retrievable via the public API.
        assert_eq!(store.get_receipt(&tx1).unwrap(), Some(vec![1]));
        assert_eq!(store.get_receipt(&tx2).unwrap(), Some(vec![2]));

        store.close().unwrap();
    }

    #[test]
    fn test_cached_store_rotate() {
        let mut store = CachedReceiptStore::new(Box::new(MockBackend::new()), 10);

        // Add receipt at block 1. maybe_rotate sets next_rotate_block = 1+10 = 11.
        let tx1 = [0x01; 32];
        store.set_receipts(1, &[ReceiptRecord { tx_hash: tx1, receipt_bytes: vec![1] }]).unwrap();
        assert!(store.cache.get_receipt(&tx1).is_some());

        // Block 11 — triggers first rotation. next_rotate_block becomes 21.
        let tx2 = [0x02; 32];
        store.set_receipts(11, &[ReceiptRecord { tx_hash: tx2, receipt_bytes: vec![2] }]).unwrap();

        // tx1 should still be in cache (only one rotation so far, keeps 2 chunks).
        assert!(store.cache.get_receipt(&tx1).is_some());

        // Block 21 — triggers second rotation. next_rotate_block becomes 31.
        let tx3 = [0x03; 32];
        store.set_receipts(21, &[ReceiptRecord { tx_hash: tx3, receipt_bytes: vec![3] }]).unwrap();

        // tx1 should now be pruned from cache after two rotations.
        assert!(store.cache.get_receipt(&tx1).is_none());
        // But still available via backend fallback.
        assert_eq!(store.get_receipt(&tx1).unwrap(), Some(vec![1]));
        // Recent data still in cache.
        assert!(store.cache.get_receipt(&tx3).is_some());

        store.close().unwrap();
    }

    #[test]
    fn test_cached_store_version_delegation() {
        let mut store = CachedReceiptStore::new(Box::new(MockBackend::new()), 500);

        assert_eq!(store.latest_version(), 0);
        store.set_latest_version(42).unwrap();
        assert_eq!(store.latest_version(), 42);

        store.set_earliest_version(5).unwrap();

        store.close().unwrap();
    }
}
