use mptdb_common::error::Result;

/// Receipt type decoupled from Cosmos SDK.
#[derive(Clone, Debug)]
pub struct Receipt {
    pub tx_hash: [u8; 32],
    pub block_number: u64,
    pub data: Vec<u8>,
    pub logs: Vec<Log>,
}

#[derive(Clone, Debug)]
pub struct Log {
    pub address: [u8; 20],
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
    pub block_number: u64,
    pub tx_hash: [u8; 32],
    pub tx_index: u32,
    pub log_index: u32,
    pub block_hash: [u8; 32],
    pub removed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FilterCriteria {
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    pub addresses: Vec<[u8; 20]>,
    pub topics: Vec<Vec<[u8; 32]>>,
}

#[derive(Clone, Debug)]
pub struct ReceiptRecord {
    pub tx_hash: [u8; 32],
    pub receipt_bytes: Vec<u8>,
}

/// ReceiptStore trait — backend abstraction for receipt storage.
pub trait ReceiptStore: Send + Sync {
    fn latest_version(&self) -> i64;
    fn set_latest_version(&self, version: i64) -> Result<()>;
    fn set_earliest_version(&self, version: i64) -> Result<()>;
    fn get_receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<Vec<u8>>>;
    fn set_receipts(&self, block_height: u64, receipts: &[ReceiptRecord]) -> Result<()>;
    fn filter_logs(&self, filter: &FilterCriteria) -> Result<Vec<Log>>;
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_criteria_default() {
        let filter = FilterCriteria::default();
        assert!(filter.from_block.is_none());
        assert!(filter.to_block.is_none());
        assert!(filter.addresses.is_empty());
        assert!(filter.topics.is_empty());
    }

    #[test]
    fn test_receipt_record_creation() {
        let tx_hash = [0xabu8; 32];
        let receipt_bytes = vec![1, 2, 3, 4];
        let record = ReceiptRecord { tx_hash, receipt_bytes: receipt_bytes.clone() };
        assert_eq!(record.tx_hash, [0xab; 32]);
        assert_eq!(record.receipt_bytes, receipt_bytes);
    }

    #[test]
    fn test_log_creation() {
        let log = Log {
            address: [0x01; 20],
            topics: vec![[0x02; 32], [0x03; 32]],
            data: vec![0xff, 0xfe],
            block_number: 42,
            tx_hash: [0x04; 32],
            tx_index: 0,
            log_index: 3,
            block_hash: [0x05; 32],
            removed: false,
        };
        assert_eq!(log.address, [0x01; 20]);
        assert_eq!(log.topics.len(), 2);
        assert_eq!(log.block_number, 42);
        assert_eq!(log.log_index, 3);
        assert!(!log.removed);
    }
}
