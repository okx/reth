use mptdb_common::config::ReceiptStoreConfig;
use mptdb_ledger::{
    factory::new_receipt_store,
    types::{FilterCriteria, ReceiptRecord},
};
use tempfile::tempdir;

/// Helper: create a receipt store config pointing at the given directory with no pruning.
fn test_config(dir: &std::path::Path) -> ReceiptStoreConfig {
    ReceiptStoreConfig {
        db_directory: dir.to_string_lossy().to_string(),
        keep_recent: 0,
        prune_interval_seconds: 0,
        ..Default::default()
    }
}

#[test]
fn test_receipt_write_read() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    let tx_hash = [0xab; 32];
    let data = vec![1, 2, 3, 4, 5];
    store.set_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

    let result = store.get_receipt(&tx_hash).unwrap();
    assert_eq!(result, Some(data));

    // Non-existent hash returns None.
    let missing = [0xff; 32];
    assert_eq!(store.get_receipt(&missing).unwrap(), None);

    store.close().unwrap();
}

#[test]
fn test_receipt_multi_block() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    // Write receipts across 5 blocks.
    for block in 1..=5u64 {
        let mut tx_hash = [0u8; 32];
        tx_hash[0] = block as u8;
        store
            .set_receipts(block, &[ReceiptRecord { tx_hash, receipt_bytes: vec![block as u8; 4] }])
            .unwrap();
    }

    // All should be retrievable.
    for block in 1..=5u64 {
        let mut tx_hash = [0u8; 32];
        tx_hash[0] = block as u8;
        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(vec![block as u8; 4]));
    }

    store.close().unwrap();
}

#[test]
fn test_receipt_cache_hit() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    let tx_hash = [0xcc; 32];
    let data = vec![10, 20, 30];

    store.set_receipts(1, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

    // First read populates/confirms cache.
    let r1 = store.get_receipt(&tx_hash).unwrap();
    assert_eq!(r1, Some(data.clone()));

    // Second read should also succeed (cache hit path).
    let r2 = store.get_receipt(&tx_hash).unwrap();
    assert_eq!(r2, Some(data));

    store.close().unwrap();
}

#[test]
fn test_receipt_genesis_block() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    let tx_hash = [0xdd; 32];
    let data = vec![0xde, 0xad];

    // Block height 0 (genesis) should work.
    store.set_receipts(0, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();

    let result = store.get_receipt(&tx_hash).unwrap();
    assert_eq!(result, Some(data));

    // Genesis maps to version 1 internally.
    assert_eq!(store.latest_version(), 1);

    store.close().unwrap();
}

#[test]
fn test_receipt_close_reopen() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());

    let tx_hash = [0xee; 32];
    let data = vec![42, 43, 44];

    // Write and close.
    {
        let mut store = new_receipt_store(&config, "", None).unwrap();
        store.set_receipts(5, &[ReceiptRecord { tx_hash, receipt_bytes: data.clone() }]).unwrap();
        store.close().unwrap();
    }

    // Reopen and read.
    {
        let mut store = new_receipt_store(&config, "", None).unwrap();
        let result = store.get_receipt(&tx_hash).unwrap();
        assert_eq!(result, Some(data));
        store.close().unwrap();
    }
}

#[test]
fn test_receipt_version_tracking() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    assert_eq!(store.latest_version(), 0);

    store
        .set_receipts(10, &[ReceiptRecord { tx_hash: [0x01; 32], receipt_bytes: vec![1] }])
        .unwrap();
    assert_eq!(store.latest_version(), 10);

    store
        .set_receipts(20, &[ReceiptRecord { tx_hash: [0x02; 32], receipt_bytes: vec![2] }])
        .unwrap();
    assert_eq!(store.latest_version(), 20);

    // Direct version manipulation.
    store.set_latest_version(50).unwrap();
    assert_eq!(store.latest_version(), 50);

    store.set_earliest_version(5).unwrap();

    store.close().unwrap();
}

#[test]
fn test_filter_logs_not_supported() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let store = new_receipt_store(&config, "", None).unwrap();

    // The MVCC backend does not support range-based log filtering.
    // The cached layer merges cache logs with backend logs; since the backend
    // returns an error it falls back to cache-only (which is empty here).
    let filter = FilterCriteria::default();
    let result = store.filter_logs(&filter).unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_factory_full_lifecycle() {
    let dir = tempdir().unwrap();
    let config = test_config(dir.path());
    let mut store = new_receipt_store(&config, "", None).unwrap();

    // Write multiple blocks.
    for block in 1..=3u64 {
        let mut hash = [0u8; 32];
        hash[0] = block as u8;
        store
            .set_receipts(
                block,
                &[ReceiptRecord { tx_hash: hash, receipt_bytes: vec![block as u8; 8] }],
            )
            .unwrap();
    }

    // Read back all.
    for block in 1..=3u64 {
        let mut hash = [0u8; 32];
        hash[0] = block as u8;
        let data = store.get_receipt(&hash).unwrap();
        assert_eq!(data, Some(vec![block as u8; 8]));
    }

    // Version should reflect last block written.
    assert_eq!(store.latest_version(), 3);

    // Close cleanly.
    store.close().unwrap();
}
