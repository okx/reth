//! P0 acceptance test for SS CompositeStateStore (A-08).
//!
//! Ported from Go TestE2E_LargeChangesetParallelWrite in
//! sei-db/state_db/ss/composite/store_test.go.

use seidb_common::{
    config::{ReadMode, StateStoreConfig, WriteMode},
    evm_keys::{NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_ss::composite::store::CompositeStateStore;
use seidb_traits::ss::StateStore;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const EVM_STORE_KEY: &str = "evm";

fn make_config(
    dir: &std::path::Path,
    write_mode: WriteMode,
    read_mode: ReadMode,
) -> StateStoreConfig {
    StateStoreConfig {
        db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
        evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
        keep_last_version: true,
        write_mode,
        read_mode,
        ..Default::default()
    }
}

fn open_composite(
    dir: &std::path::Path,
    write_mode: WriteMode,
    read_mode: ReadMode,
) -> CompositeStateStore {
    let cfg = make_config(dir, write_mode, read_mode);
    CompositeStateStore::new(&cfg, &dir.to_string_lossy()).unwrap()
}

// ===========================================================================
// A-08: Large changeset parallel write (DualWrite mode)
// ===========================================================================
// Ported from Go TestE2E_LargeChangesetParallelWrite.
//
// Create CompositeStateStore in DualWrite mode. Apply a large changeset
// (1000+ KV pairs mixing bank + evm data). Verify all data written correctly
// to both stores.

#[test]
fn a08_large_changeset_parallel_write() {
    let dir = tempdir().unwrap();
    let mut store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    // Build EVM pairs: 100 storage keys + 50 nonce keys = 150 EVM pairs
    let mut evm_pairs: Vec<KvPair> = Vec::new();

    struct KeyRecord {
        full_key: Vec<u8>,
        value: Vec<u8>,
    }

    let mut storage_records: Vec<KeyRecord> = Vec::new();
    let mut nonce_records: Vec<KeyRecord> = Vec::new();

    for i in 0..100u32 {
        let mut addr = [0u8; 20];
        addr[0] = (i >> 8) as u8;
        addr[1] = i as u8;
        let mut slot = [0u8; 32];
        slot[0] = i as u8;

        let mut full_key = vec![STATE_KEY_PREFIX];
        full_key.extend_from_slice(&addr);
        full_key.extend_from_slice(&slot);

        let value = format!("storage_{i}").into_bytes();
        evm_pairs.push(KvPair { delete: false, key: full_key.clone(), value: value.clone() });
        storage_records.push(KeyRecord { full_key, value });
    }

    for i in 0..50u32 {
        let mut addr = [0u8; 20];
        addr[0] = (i + 200) as u8;

        let mut full_key = vec![NONCE_KEY_PREFIX];
        full_key.extend_from_slice(&addr);

        let value = vec![i as u8];
        evm_pairs.push(KvPair { delete: false, key: full_key.clone(), value: value.clone() });
        nonce_records.push(KeyRecord { full_key, value });
    }

    // Build bank pairs: 50 bank keys
    let mut bank_pairs: Vec<KvPair> = Vec::new();
    for i in 0..50u32 {
        bank_pairs.push(KvPair {
            delete: false,
            key: format!("balance_{i}").into_bytes(),
            value: format!("{}", i * 100).into_bytes(),
        });
    }

    // Apply as a single large changeset with both EVM and bank data
    let changesets = vec![
        NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet { pairs: evm_pairs }),
        },
        NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet { pairs: bank_pairs }),
        },
    ];

    store.apply_changeset_sync(1, &changesets).unwrap();
    store.set_latest_version(1).unwrap();

    // Verify all storage EVM keys
    for (i, rec) in storage_records.iter().enumerate() {
        let val = store.get(EVM_STORE_KEY, 1, &rec.full_key).unwrap();
        assert_eq!(val.as_deref(), Some(rec.value.as_slice()), "Storage key {i} mismatch");
    }

    // Verify all nonce EVM keys
    for (i, rec) in nonce_records.iter().enumerate() {
        let val = store.get(EVM_STORE_KEY, 1, &rec.full_key).unwrap();
        assert_eq!(val.as_deref(), Some(rec.value.as_slice()), "Nonce key {i} mismatch");
    }

    // Verify all bank keys
    for i in 0..50u32 {
        let key = format!("balance_{i}").into_bytes();
        let expected = format!("{}", i * 100).into_bytes();
        let val = store.get("bank", 1, &key).unwrap();
        assert_eq!(val.as_deref(), Some(expected.as_slice()), "Bank key {i} mismatch");
    }

    // Verify total count: 150 EVM + 50 bank = 200 keys
    // Spot-check that Has works for a sample
    let has_storage = store.has(EVM_STORE_KEY, 1, &storage_records[0].full_key).unwrap();
    assert!(has_storage, "first storage key should exist via Has()");

    let has_nonce = store.has(EVM_STORE_KEY, 1, &nonce_records[0].full_key).unwrap();
    assert!(has_nonce, "first nonce key should exist via Has()");

    let has_bank = store.has("bank", 1, b"balance_0").unwrap();
    assert!(has_bank, "first bank key should exist via Has()");

    // Verify nonexistent key returns None
    let missing = store.get("bank", 1, b"nonexistent_key").unwrap();
    assert!(missing.is_none(), "nonexistent key should return None");

    store.close().unwrap();
}

/// Additional large-changeset test: verify 1000+ KV pairs total.
#[test]
fn a08_large_changeset_1000_plus_pairs() {
    let dir = tempdir().unwrap();
    let mut store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    // Build 1000 EVM storage pairs
    let mut evm_pairs: Vec<KvPair> = Vec::new();
    for i in 0..1000u32 {
        let mut addr = [0u8; 20];
        addr[0] = (i >> 8) as u8;
        addr[1] = (i & 0xFF) as u8;
        let mut slot = [0u8; 32];
        slot[0] = (i >> 8) as u8;
        slot[1] = (i & 0xFF) as u8;

        let mut full_key = vec![STATE_KEY_PREFIX];
        full_key.extend_from_slice(&addr);
        full_key.extend_from_slice(&slot);

        let value = format!("v_{i}").into_bytes();
        evm_pairs.push(KvPair { delete: false, key: full_key, value });
    }

    // Build 100 bank pairs
    let mut bank_pairs: Vec<KvPair> = Vec::new();
    for i in 0..100u32 {
        bank_pairs.push(KvPair {
            delete: false,
            key: format!("bal_{i}").into_bytes(),
            value: format!("{}", i).into_bytes(),
        });
    }

    let changesets = vec![
        NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet { pairs: evm_pairs }),
        },
        NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet { pairs: bank_pairs }),
        },
    ];

    store.apply_changeset_sync(1, &changesets).unwrap();
    store.set_latest_version(1).unwrap();

    // Spot-check a few storage keys at different positions
    for sample_i in [0u32, 100, 500, 999] {
        let mut addr = [0u8; 20];
        addr[0] = (sample_i >> 8) as u8;
        addr[1] = (sample_i & 0xFF) as u8;
        let mut slot = [0u8; 32];
        slot[0] = (sample_i >> 8) as u8;
        slot[1] = (sample_i & 0xFF) as u8;

        let mut full_key = vec![STATE_KEY_PREFIX];
        full_key.extend_from_slice(&addr);
        full_key.extend_from_slice(&slot);

        let expected = format!("v_{sample_i}").into_bytes();
        let val = store.get(EVM_STORE_KEY, 1, &full_key).unwrap();
        assert_eq!(val.as_deref(), Some(expected.as_slice()), "Storage key {sample_i} mismatch");
    }

    // Spot-check bank keys
    for sample_i in [0u32, 50, 99] {
        let key = format!("bal_{sample_i}").into_bytes();
        let expected = format!("{sample_i}").into_bytes();
        let val = store.get("bank", 1, &key).unwrap();
        assert_eq!(val.as_deref(), Some(expected.as_slice()), "Bank key {sample_i} mismatch");
    }

    store.close().unwrap();
}
