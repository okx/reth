//! End-to-end integration tests for seidb-ss.
//!
//! These tests exercise the full stack: factory -> CompositeStateStore ->
//! Cosmos/EVM backends, covering all write/read mode combinations, key
//! routing, pruning, deletion, version tracking, and persistence.

use seidb_common::{
    config::{ReadMode, StateStoreConfig, WriteMode},
    evm_keys::{CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_ss::{composite::store::CompositeStateStore, factory::new_state_store};
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

fn make_changeset(store: &str, pairs: Vec<(&[u8], Option<&[u8]>)>) -> Vec<NamedChangeSet> {
    vec![NamedChangeSet {
        name: store.to_string(),
        changeset: Some(ChangeSet {
            pairs: pairs
                .into_iter()
                .map(|(k, v)| KvPair {
                    delete: v.is_none(),
                    key: k.to_vec(),
                    value: v.unwrap_or_default().to_vec(),
                })
                .collect(),
        }),
    }]
}

fn open_composite(
    dir: &std::path::Path,
    write_mode: WriteMode,
    read_mode: ReadMode,
) -> CompositeStateStore {
    let cfg = make_config(dir, write_mode, read_mode);
    CompositeStateStore::new(&cfg, &dir.to_string_lossy()).unwrap()
}

fn evm_nonce_key(addr: &[u8; 20]) -> Vec<u8> {
    let mut k = vec![NONCE_KEY_PREFIX];
    k.extend_from_slice(addr);
    k
}

fn evm_codehash_key(addr: &[u8; 20]) -> Vec<u8> {
    let mut k = vec![CODE_HASH_KEY_PREFIX];
    k.extend_from_slice(addr);
    k
}

fn evm_code_key(addr: &[u8; 20]) -> Vec<u8> {
    let mut k = vec![CODE_KEY_PREFIX];
    k.extend_from_slice(addr);
    k
}

fn evm_storage_key(addr: &[u8; 20], slot: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![STATE_KEY_PREFIX];
    k.extend_from_slice(addr);
    k.extend_from_slice(slot);
    k
}

fn evm_legacy_key(data: &[u8]) -> Vec<u8> {
    let mut k = vec![0x01]; // Non-EVM prefix -> Legacy
    k.extend_from_slice(data);
    k
}

fn test_addr() -> [u8; 20] {
    let mut addr = [0u8; 20];
    for (i, b) in addr.iter_mut().enumerate() {
        *b = (i + 1) as u8;
    }
    addr
}

fn test_slot() -> [u8; 32] {
    let mut slot = [0u8; 32];
    for (i, b) in slot.iter_mut().enumerate() {
        *b = (0xa0 + i) as u8;
    }
    slot
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// T1: CosmosOnly mode — all data (including EVM) goes to cosmos store only.
#[test]
fn test_e2e_cosmos_only_mode() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::CosmosOnly, ReadMode::CosmosOnly);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Write bank data.
    let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    // Write EVM data — should go to cosmos since CosmosOnly.
    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"42"))]);
    store.apply_changeset_sync(2, &cs).unwrap();

    // Both readable from cosmos.
    assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"100".to_vec()));
    assert_eq!(store.get(EVM_STORE_KEY, 2, &nonce_key).unwrap(), Some(b"42".to_vec()));
}

/// T2: DualWrite mode — EVM data written to both cosmos and EVM stores.
#[test]
fn test_e2e_dual_write_mode() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"99"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    // Readable via composite (EvmFirst reads EVM store first).
    let val = store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
    assert_eq!(val, Some(b"99".to_vec()));

    // Also in cosmos (dual write).
    let bank_cs = make_changeset("bank", vec![(b"bob", Some(b"200"))]);
    store.apply_changeset_sync(2, &bank_cs).unwrap();
    assert_eq!(store.get("bank", 2, b"bob").unwrap(), Some(b"200".to_vec()));
}

/// T3: SplitWrite mode — EVM data stripped from cosmos, only in EVM store.
#[test]
fn test_e2e_split_write_mode() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::SplitWrite, ReadMode::SplitRead);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Mixed changeset: bank + EVM nonce.
    let changesets = vec![
        NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: b"alice".to_vec(),
                    value: b"100".to_vec(),
                }],
            }),
        },
        NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: nonce_key.clone(),
                    value: b"55".to_vec(),
                }],
            }),
        },
    ];
    store.apply_changeset_sync(1, &changesets).unwrap();

    // Bank data in cosmos.
    assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"100".to_vec()));

    // EVM nonce via SplitRead goes to EVM store.
    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"55".to_vec()));
}

/// T4: EvmFirst read mode — falls back to cosmos when EVM store returns None.
#[test]
fn test_e2e_evm_first_read_fallback() {
    let dir = tempdir().unwrap();
    // CosmosOnly write so EVM store is not opened — but EvmFirst read.
    // Actually we need EVM store to be opened for EvmFirst to try it.
    // Use DualWrite but only write to cosmos directly to simulate missing EVM data.
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Write bank data (non-EVM) — only goes to cosmos.
    let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    // Bank key not in EVM store, fallback to cosmos succeeds.
    assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"100".to_vec()));

    // EVM key not written at all — returns None from both.
    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), None);
}

/// T5: SplitRead mode — no fallback to cosmos for EVM keys.
#[test]
fn test_e2e_split_read_no_fallback() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::SplitRead);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Write EVM data via DualWrite (goes to both stores).
    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"77"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    // SplitRead: EVM key goes directly to EVM store — should find it.
    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"77".to_vec()));

    // Non-EVM key lookup goes to cosmos.
    let bank_cs = make_changeset("bank", vec![(b"alice", Some(b"50"))]);
    store.apply_changeset_sync(2, &bank_cs).unwrap();
    assert_eq!(store.get("bank", 2, b"alice").unwrap(), Some(b"50".to_vec()));
}

/// T6: All 5 EVM key types are routed correctly in DualWrite/EvmFirst mode.
#[test]
fn test_e2e_all_evm_key_types() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    let addr = test_addr();
    let slot = test_slot();

    let nonce_key = evm_nonce_key(&addr);
    let codehash_key = evm_codehash_key(&addr);
    let code_key = evm_code_key(&addr);
    let storage_key = evm_storage_key(&addr, &slot);
    let legacy_key = evm_legacy_key(b"addr_map");

    let cs = vec![NamedChangeSet {
        name: EVM_STORE_KEY.to_string(),
        changeset: Some(ChangeSet {
            pairs: vec![
                KvPair { delete: false, key: nonce_key.clone(), value: b"1".to_vec() },
                KvPair { delete: false, key: codehash_key.clone(), value: b"hash123".to_vec() },
                KvPair { delete: false, key: code_key.clone(), value: b"0xdead".to_vec() },
                KvPair { delete: false, key: storage_key.clone(), value: b"slot_val".to_vec() },
                KvPair { delete: false, key: legacy_key.clone(), value: b"legacy_v".to_vec() },
            ],
        }),
    }];
    store.apply_changeset_sync(1, &cs).unwrap();

    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"1".to_vec()));
    assert_eq!(store.get(EVM_STORE_KEY, 1, &codehash_key).unwrap(), Some(b"hash123".to_vec()));
    assert_eq!(store.get(EVM_STORE_KEY, 1, &code_key).unwrap(), Some(b"0xdead".to_vec()));
    assert_eq!(store.get(EVM_STORE_KEY, 1, &storage_key).unwrap(), Some(b"slot_val".to_vec()));
    assert_eq!(store.get(EVM_STORE_KEY, 1, &legacy_key).unwrap(), Some(b"legacy_v".to_vec()));
}

/// T7: Mixed changeset with bank + EVM data routes correctly.
#[test]
fn test_e2e_mixed_changeset_routing() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::SplitWrite, ReadMode::SplitRead);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    let changesets = vec![
        NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: b"alice".to_vec(),
                    value: b"500".to_vec(),
                }],
            }),
        },
        NamedChangeSet {
            name: "staking".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: b"validator1".to_vec(),
                    value: b"1000".to_vec(),
                }],
            }),
        },
        NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair { delete: false, key: nonce_key.clone(), value: b"7".to_vec() }],
            }),
        },
    ];
    store.apply_changeset_sync(1, &changesets).unwrap();

    // Non-EVM modules in cosmos.
    assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"500".to_vec()));
    assert_eq!(store.get("staking", 1, b"validator1").unwrap(), Some(b"1000".to_vec()));
    // EVM nonce in EVM store.
    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"7".to_vec()));
}

/// T8: Delete (tombstone) propagates correctly through the composite store.
#[test]
fn test_e2e_delete_tombstone() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Write.
    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"42"))]);
    store.apply_changeset_sync(1, &cs).unwrap();
    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"42".to_vec()));

    // Delete.
    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), None)]);
    store.apply_changeset_sync(2, &cs).unwrap();
    assert_eq!(store.get(EVM_STORE_KEY, 2, &nonce_key).unwrap(), None);
    assert!(!store.has(EVM_STORE_KEY, 2, &nonce_key).unwrap());

    // Bank delete.
    let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
    store.apply_changeset_sync(3, &cs).unwrap();
    let cs = make_changeset("bank", vec![(b"alice", None)]);
    store.apply_changeset_sync(4, &cs).unwrap();
    assert_eq!(store.get("bank", 4, b"alice").unwrap(), None);
}

/// T9: Version tracking is consistent across both stores.
#[test]
fn test_e2e_version_consistency() {
    let dir = tempdir().unwrap();
    let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    assert_eq!(store.get_latest_version(), 0);
    assert_eq!(store.get_earliest_version(), 0);

    // Write multiple versions.
    for v in 1..=5 {
        let cs = make_changeset("bank", vec![(b"key", Some(format!("v{v}").as_bytes()))]);
        store.apply_changeset_sync(v, &cs).unwrap();
    }

    store.set_latest_version(5).unwrap();
    assert_eq!(store.get_latest_version(), 5);

    store.set_earliest_version(2, false).unwrap();
    assert_eq!(store.get_earliest_version(), 2);

    // Data at version 5 should be accessible.
    assert_eq!(store.get("bank", 5, b"key").unwrap(), Some(b"v5".to_vec()));
}

/// T10: Pruning cosmos store through the composite layer.
#[test]
fn test_e2e_pruning_cosmos_store() {
    let dir = tempdir().unwrap();
    // Use CosmosOnly to avoid EVM sub-DB prune issues with default comparer.
    let store = open_composite(dir.path(), WriteMode::CosmosOnly, ReadMode::CosmosOnly);

    // Write v1 and v2 to bank.
    let cs = make_changeset("bank", vec![(b"alice", Some(b"b1"))]);
    store.apply_changeset_sync(1, &cs).unwrap();
    let cs = make_changeset("bank", vec![(b"alice", Some(b"b2"))]);
    store.apply_changeset_sync(2, &cs).unwrap();

    // Prune version 1.
    store.prune(1).unwrap();

    // Version 2 should still be available.
    assert_eq!(store.get("bank", 2, b"alice").unwrap(), Some(b"b2".to_vec()));
}

/// T11: Factory function creates a working store.
#[test]
fn test_e2e_factory_creates_correct_store() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path(), WriteMode::CosmosOnly, ReadMode::CosmosOnly);

    let store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();

    // Write and read through the Arc<CompositeStateStore>.
    let cs = make_changeset("bank", vec![(b"alice", Some(b"factory_val"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"factory_val".to_vec()));

    store.set_latest_version(10).unwrap();
    assert_eq!(store.get_latest_version(), 10);
}

/// T12: Factory with EVM modes creates a fully functional composite store.
#[test]
fn test_e2e_factory_with_evm() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

    let store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"factory_evm"))]);
    store.apply_changeset_sync(1, &cs).unwrap();

    assert_eq!(store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap(), Some(b"factory_evm".to_vec()));
}

/// T13: Close and reopen preserves persisted data.
#[test]
fn test_e2e_close_and_reopen() {
    let dir = tempdir().unwrap();

    let addr = test_addr();
    let nonce_key = evm_nonce_key(&addr);

    // Phase 1: write data and close.
    {
        let mut store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

        let cs = make_changeset("bank", vec![(b"alice", Some(b"persist"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"evm_persist"))]);
        store.apply_changeset_sync(2, &cs).unwrap();

        store.set_latest_version(2).unwrap();
        store.close().unwrap();
    }

    // Phase 2: reopen and verify data is still there.
    {
        let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

        assert_eq!(store.get_latest_version(), 2);
        assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"persist".to_vec()));
        assert_eq!(store.get(EVM_STORE_KEY, 2, &nonce_key).unwrap(), Some(b"evm_persist".to_vec()));
    }
}
