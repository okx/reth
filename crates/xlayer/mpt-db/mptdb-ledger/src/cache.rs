use crate::types::{FilterCriteria, Log, ReceiptRecord};
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};

const NUM_CACHE_CHUNKS: usize = 3;

/// 3-chunk rotation cache for receipts and logs.
///
/// Keeps the two most-recent chunks for reads and prunes the oldest on rotation.
/// This mirrors the Go `ledgerCache` in mpt-db.
pub struct LedgerCache {
    receipt_chunks: [RwLock<ReceiptChunk>; NUM_CACHE_CHUNKS],
    receipt_write_slot: AtomicUsize,
    log_chunks: [RwLock<LogChunk>; NUM_CACHE_CHUNKS],
    log_write_slot: AtomicUsize,
}

struct ReceiptChunk {
    /// blockNumber -> (txHash -> receiptBytes)
    receipts: HashMap<u64, HashMap<[u8; 32], Vec<u8>>>,
    /// txHash -> blockNumber (reverse index for fast lookup)
    receipt_index: HashMap<[u8; 32], u64>,
}

struct LogChunk {
    /// blockNumber -> logs
    logs: HashMap<u64, Vec<Log>>,
}

impl ReceiptChunk {
    fn new() -> Self {
        Self { receipts: HashMap::new(), receipt_index: HashMap::new() }
    }

    fn clear(&mut self) {
        self.receipts.clear();
        self.receipt_index.clear();
    }
}

impl LogChunk {
    fn new() -> Self {
        Self { logs: HashMap::new() }
    }

    fn clear(&mut self) {
        self.logs.clear();
    }
}

impl LedgerCache {
    /// Create a new cache with 3 empty chunks for receipts and logs.
    pub fn new() -> Self {
        Self {
            receipt_chunks: [
                RwLock::new(ReceiptChunk::new()),
                RwLock::new(ReceiptChunk::new()),
                RwLock::new(ReceiptChunk::new()),
            ],
            receipt_write_slot: AtomicUsize::new(0),
            log_chunks: [
                RwLock::new(LogChunk::new()),
                RwLock::new(LogChunk::new()),
                RwLock::new(LogChunk::new()),
            ],
            log_write_slot: AtomicUsize::new(0),
        }
    }

    /// Rotate the cache: advance write slot, clear the oldest chunk (which becomes the new
    /// write target).
    pub fn rotate(&self) {
        // Rotate receipts
        let old_receipt = self.receipt_write_slot.load(Ordering::Acquire);
        let new_receipt = (old_receipt + 1) % NUM_CACHE_CHUNKS;
        let prune_receipt = (new_receipt + 1) % NUM_CACHE_CHUNKS;
        // Clear the new write slot (it was the oldest readable chunk)
        self.receipt_chunks[new_receipt].write().clear();
        self.receipt_write_slot.store(new_receipt, Ordering::Release);
        // Prune the oldest chunk
        self.receipt_chunks[prune_receipt].write().clear();

        // Rotate logs
        let old_log = self.log_write_slot.load(Ordering::Acquire);
        let new_log = (old_log + 1) % NUM_CACHE_CHUNKS;
        let prune_log = (new_log + 1) % NUM_CACHE_CHUNKS;
        self.log_chunks[new_log].write().clear();
        self.log_write_slot.store(new_log, Ordering::Release);
        self.log_chunks[prune_log].write().clear();
    }

    /// Look up a receipt by transaction hash across all chunks.
    /// Returns the raw receipt bytes if found.
    pub fn get_receipt(&self, tx_hash: &[u8; 32]) -> Option<Vec<u8>> {
        let write_slot = self.receipt_write_slot.load(Ordering::Acquire);
        for i in 0..NUM_CACHE_CHUNKS {
            let slot = (write_slot + NUM_CACHE_CHUNKS - i) % NUM_CACHE_CHUNKS;
            let chunk = self.receipt_chunks[slot].read();
            if let Some(&block_num) = chunk.receipt_index.get(tx_hash) &&
                let Some(block_receipts) = chunk.receipts.get(&block_num) &&
                let Some(data) = block_receipts.get(tx_hash)
            {
                return Some(data.clone());
            }
        }
        None
    }

    /// Add a batch of receipts for a given block into the current write chunk.
    pub fn add_receipts_batch(&self, block_number: u64, receipts: &[ReceiptRecord]) {
        if receipts.is_empty() {
            return;
        }
        let slot = self.receipt_write_slot.load(Ordering::Acquire);
        let mut chunk = self.receipt_chunks[slot].write();
        let block_map = chunk
            .receipts
            .entry(block_number)
            .or_insert_with(|| HashMap::with_capacity(receipts.len()));
        for r in receipts {
            block_map.insert(r.tx_hash, r.receipt_bytes.clone());
        }
        for r in receipts {
            chunk.receipt_index.insert(r.tx_hash, block_number);
        }
    }

    /// Add logs for a block into the current write chunk.
    pub fn add_logs_for_block(&self, block_number: u64, logs: &[Log]) {
        if logs.is_empty() {
            return;
        }
        let slot = self.log_write_slot.load(Ordering::Acquire);
        let mut chunk = self.log_chunks[slot].write();
        chunk.logs.insert(block_number, logs.to_vec());
    }

    /// Return all cached logs matching the given filter criteria.
    pub fn filter_logs(&self, filter: &FilterCriteria) -> Vec<Log> {
        let mut result = Vec::new();
        for i in 0..NUM_CACHE_CHUNKS {
            let chunk = self.log_chunks[i].read();
            for (&block_num, logs) in &chunk.logs {
                if let Some(from) = filter.from_block &&
                    block_num < from
                {
                    continue;
                }
                if let Some(to) = filter.to_block &&
                    block_num > to
                {
                    continue;
                }
                for log in logs {
                    if Self::match_log(log, filter) {
                        result.push(log.clone());
                    }
                }
            }
        }
        result
    }

    /// Check whether a single log matches the filter criteria.
    pub fn match_log(log: &Log, filter: &FilterCriteria) -> bool {
        // Address filter
        if !filter.addresses.is_empty() && !filter.addresses.contains(&log.address) {
            return false;
        }

        // Topic filters
        for (i, topic_list) in filter.topics.iter().enumerate() {
            if topic_list.is_empty() {
                continue; // wildcard
            }
            if i >= log.topics.len() {
                return false;
            }
            if !topic_list.contains(&log.topics[i]) {
                return false;
            }
        }

        // Block range
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

        true
    }
}

impl Default for LedgerCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FilterCriteria, Log, ReceiptRecord};

    fn make_log(address: [u8; 20], topics: Vec<[u8; 32]>, block_number: u64) -> Log {
        Log {
            address,
            topics,
            data: vec![],
            block_number,
            tx_hash: [0u8; 32],
            tx_index: 0,
            log_index: 0,
            block_hash: [0u8; 32],
            removed: false,
        }
    }

    #[test]
    fn test_cache_add_get_receipt() {
        let cache = LedgerCache::new();
        let tx_hash = [0xabu8; 32];
        let data = vec![1, 2, 3, 4];
        let records = vec![ReceiptRecord { tx_hash, receipt_bytes: data.clone() }];
        cache.add_receipts_batch(100, &records);

        let result = cache.get_receipt(&tx_hash);
        assert_eq!(result, Some(data));
    }

    #[test]
    fn test_cache_receipt_not_found() {
        let cache = LedgerCache::new();
        let missing = [0xffu8; 32];
        assert_eq!(cache.get_receipt(&missing), None);
    }

    #[test]
    fn test_cache_rotate_clears_oldest() {
        let cache = LedgerCache::new();

        // Add receipt in slot 0
        let tx0 = [0x00u8; 32];
        cache.add_receipts_batch(1, &[ReceiptRecord { tx_hash: tx0, receipt_bytes: vec![10] }]);
        assert!(cache.get_receipt(&tx0).is_some());

        // Rotate 1: write_slot -> 1, prune slot 2 (empty). Slot 0 still readable.
        cache.rotate();
        assert!(cache.get_receipt(&tx0).is_some());

        // Add receipt in slot 1
        let tx1 = [0x01u8; 32];
        cache.add_receipts_batch(2, &[ReceiptRecord { tx_hash: tx1, receipt_bytes: vec![20] }]);

        // Rotate 2: write_slot -> 2, prune slot 0. tx0 should be gone.
        cache.rotate();
        assert!(cache.get_receipt(&tx0).is_none(), "oldest chunk should be pruned");
        assert!(cache.get_receipt(&tx1).is_some(), "previous chunk should still be readable");

        // Rotate 3: write_slot -> 0, prune slot 1. tx1 should be gone.
        cache.rotate();
        assert!(cache.get_receipt(&tx1).is_none(), "tx1 should be pruned after 3rd rotation");
    }

    #[test]
    fn test_cache_filter_logs_address() {
        let cache = LedgerCache::new();
        let addr_a = [0x0au8; 20];
        let addr_b = [0x0bu8; 20];
        cache.add_logs_for_block(10, &[make_log(addr_a, vec![], 10), make_log(addr_b, vec![], 10)]);

        let filter = FilterCriteria { addresses: vec![addr_a], ..Default::default() };
        let result = cache.filter_logs(&filter);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].address, addr_a);
    }

    #[test]
    fn test_cache_filter_logs_topic() {
        let cache = LedgerCache::new();
        let topic_a = [0x01u8; 32];
        let topic_b = [0x02u8; 32];
        cache.add_logs_for_block(
            10,
            &[make_log([0u8; 20], vec![topic_a], 10), make_log([0u8; 20], vec![topic_b], 10)],
        );

        let filter = FilterCriteria { topics: vec![vec![topic_a]], ..Default::default() };
        let result = cache.filter_logs(&filter);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].topics[0], topic_a);
    }

    #[test]
    fn test_cache_filter_logs_block_range() {
        let cache = LedgerCache::new();
        cache.add_logs_for_block(5, &[make_log([0u8; 20], vec![], 5)]);
        cache.add_logs_for_block(10, &[make_log([0u8; 20], vec![], 10)]);
        cache.add_logs_for_block(15, &[make_log([0u8; 20], vec![], 15)]);

        let filter =
            FilterCriteria { from_block: Some(8), to_block: Some(12), ..Default::default() };
        let result = cache.filter_logs(&filter);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].block_number, 10);
    }

    #[test]
    fn test_cache_match_log_wildcard() {
        let topic_a = [0x01u8; 32];
        let topic_b = [0x02u8; 32];
        let log = make_log([0u8; 20], vec![topic_a, topic_b], 10);

        // Empty topic list at position 0 = wildcard, match topic_b at position 1
        let filter = FilterCriteria { topics: vec![vec![], vec![topic_b]], ..Default::default() };
        assert!(LedgerCache::match_log(&log, &filter));

        // Wildcard at both positions
        let filter2 = FilterCriteria { topics: vec![vec![], vec![]], ..Default::default() };
        assert!(LedgerCache::match_log(&log, &filter2));
    }

    #[test]
    fn test_cache_multi_block() {
        let cache = LedgerCache::new();
        let tx1 = [0x01u8; 32];
        let tx2 = [0x02u8; 32];
        let tx3 = [0x03u8; 32];

        cache.add_receipts_batch(
            100,
            &[
                ReceiptRecord { tx_hash: tx1, receipt_bytes: vec![10] },
                ReceiptRecord { tx_hash: tx2, receipt_bytes: vec![20] },
            ],
        );
        cache.add_receipts_batch(101, &[ReceiptRecord { tx_hash: tx3, receipt_bytes: vec![30] }]);

        assert_eq!(cache.get_receipt(&tx1), Some(vec![10]));
        assert_eq!(cache.get_receipt(&tx2), Some(vec![20]));
        assert_eq!(cache.get_receipt(&tx3), Some(vec![30]));
    }
}
