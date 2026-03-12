//! End-to-end integration tests for the FlatKV CommitStore.
//!
//! Tests cover LtHash correctness, snapshot + recovery, persistence,
//! mixed operations, import, and iteration.

use seidb_common::{
    config::FlatKvConfig,
    evm_keys::{CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_sc::flatkv::{importer::KvImporter, store::CommitStore};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn new_store(dir: &str) -> CommitStore {
    let mut store = CommitStore::new(dir, FlatKvConfig::default());
    store.load_version(0).unwrap();
    store
}

fn make_evm_changeset(pairs: Vec<(Vec<u8>, Vec<u8>, bool)>) -> Vec<NamedChangeSet> {
    vec![NamedChangeSet {
        name: "evm".to_string(),
        changeset: Some(ChangeSet {
            pairs: pairs
                .into_iter()
                .map(|(k, v, del)| KvPair { delete: del, key: k, value: v })
                .collect(),
        }),
    }]
}

/// Build an EVM storage key: prefix 0x03 + addr(20) + slot(32) = 53 bytes.
fn storage_key(addr: u8, slot: u8) -> Vec<u8> {
    let mut k = vec![STATE_KEY_PREFIX];
    k.extend_from_slice(&[addr; 20]);
    k.extend_from_slice(&[slot; 32]);
    k
}

/// Build a nonce key: prefix 0x0a + addr(20).
fn nonce_key(addr: u8) -> Vec<u8> {
    let mut k = vec![NONCE_KEY_PREFIX];
    k.extend_from_slice(&[addr; 20]);
    k
}

/// Build a codehash key: prefix 0x08 + addr(20).
fn codehash_key(addr: u8) -> Vec<u8> {
    let mut k = vec![CODE_HASH_KEY_PREFIX];
    k.extend_from_slice(&[addr; 20]);
    k
}

/// Build a code key: prefix 0x07 + addr(20).
fn code_key(addr: u8) -> Vec<u8> {
    let mut k = vec![CODE_KEY_PREFIX];
    k.extend_from_slice(&[addr; 20]);
    k
}

/// Build a legacy key (prefix 0x01, arbitrary suffix).
fn legacy_key(seed: u8) -> Vec<u8> {
    vec![0x01, seed, seed, seed]
}

fn encode_nonce(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// 1. LtHash correctness
// ---------------------------------------------------------------------------

#[test]
fn test_lt_hash_incremental_vs_reopen() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    let hash_after_10;
    {
        let mut store = new_store(db_dir);
        for i in 1u8..=10 {
            let cs = make_evm_changeset(vec![(storage_key(i, i), vec![i; 8], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        hash_after_10 = store.root_hash();
        store.close().unwrap();
    }

    // Reopen and catchup via WAL; LtHash must match.
    {
        let mut store = new_store(db_dir);
        assert_eq!(store.version(), 10);
        assert_eq!(store.root_hash(), hash_after_10);
        store.close().unwrap();
    }
}

#[test]
fn test_lt_hash_new_account_no_phantom() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    let hash_before = store.root_hash();

    // Create a brand-new account (nonce only).
    let cs = make_evm_changeset(vec![(nonce_key(0xAA), encode_nonce(1), false)]);
    store.apply_change_sets(&cs).unwrap();

    let hash_after = store.root_hash();
    // Hash must have changed (MixIn happened).
    assert_ne!(hash_before, hash_after);

    // Commit and verify the hash stays the same (no spurious delta on commit).
    store.commit().unwrap();
    assert_eq!(store.root_hash(), hash_after);

    store.close().unwrap();
}

#[test]
fn test_lt_hash_multi_apply_per_block() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    // Two apply_change_sets calls before one commit.
    let cs1 = make_evm_changeset(vec![(storage_key(1, 1), b"val_a".to_vec(), false)]);
    let cs2 = make_evm_changeset(vec![(storage_key(2, 2), b"val_b".to_vec(), false)]);
    store.apply_change_sets(&cs1).unwrap();
    store.apply_change_sets(&cs2).unwrap();
    let hash_before_commit = store.root_hash();

    store.commit().unwrap();

    // Hash should not change on commit itself.
    assert_eq!(store.root_hash(), hash_before_commit);

    store.close().unwrap();
}

#[test]
fn test_lt_hash_storage_lifecycle() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());
    let key = storage_key(0x10, 0x20);

    // Add.
    let cs_add = make_evm_changeset(vec![(key.clone(), b"hello".to_vec(), false)]);
    store.apply_change_sets(&cs_add).unwrap();
    store.commit().unwrap();
    let hash_add = store.root_hash();

    // Update.
    let cs_upd = make_evm_changeset(vec![(key.clone(), b"world".to_vec(), false)]);
    store.apply_change_sets(&cs_upd).unwrap();
    store.commit().unwrap();
    let hash_upd = store.root_hash();
    assert_ne!(hash_add, hash_upd);

    // Delete.
    let cs_del = make_evm_changeset(vec![(key.clone(), vec![], true)]);
    store.apply_change_sets(&cs_del).unwrap();
    store.commit().unwrap();
    let hash_del = store.root_hash();
    assert_ne!(hash_upd, hash_del);

    store.close().unwrap();
}

#[test]
fn test_lt_hash_empty_block() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    // Commit some data first to get a non-zero hash.
    let cs = make_evm_changeset(vec![(storage_key(1, 1), b"data".to_vec(), false)]);
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();
    let hash_before = store.root_hash();

    // Empty commit should not change the hash.
    store.commit().unwrap();
    assert_eq!(store.root_hash(), hash_before);

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 2. Snapshot + recovery
// ---------------------------------------------------------------------------

#[test]
fn test_snapshot_then_reopen() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    {
        let mut store = new_store(db_dir);

        // Write 5 blocks, snapshot at 5.
        for i in 1u8..=5 {
            let cs = make_evm_changeset(vec![(storage_key(i, i), vec![i; 4], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        store.write_snapshot().unwrap();

        // Write 5 more blocks (only in WAL + working dir).
        for i in 6u8..=10 {
            let cs = make_evm_changeset(vec![(storage_key(i, i), vec![i; 4], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 10);
        store.close().unwrap();
    }

    // Reopen: should catchup via WAL from snapshot at 5 to version 10.
    {
        let mut store = new_store(db_dir);
        assert_eq!(store.version(), 10);

        // Verify data from version 10.
        let (val, found) = store.get(&storage_key(10, 10));
        assert!(found);
        assert_eq!(val.unwrap(), vec![10u8; 4]);

        store.close().unwrap();
    }
}

#[test]
fn test_multiple_snapshots() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    {
        let config = FlatKvConfig { snapshot_keep_recent: 10, ..Default::default() };
        let mut store = CommitStore::new(db_dir, config);
        store.load_version(0).unwrap();

        for block in 1u8..=15 {
            let cs = make_evm_changeset(vec![(storage_key(block, block), vec![block; 4], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();

            if block == 5 || block == 10 || block == 15 {
                store.write_snapshot().unwrap();
            }
        }
        assert_eq!(store.version(), 15);
        store.close().unwrap();
    }

    // Reopen at latest.
    {
        let mut store = new_store(db_dir);
        assert_eq!(store.version(), 15);

        let (val, found) = store.get(&storage_key(15, 15));
        assert!(found);
        assert_eq!(val.unwrap(), vec![15u8; 4]);

        store.close().unwrap();
    }
}

#[test]
fn test_rollback_and_verify() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    let hash_at_v5;
    {
        let config = FlatKvConfig { snapshot_interval: 0, ..Default::default() };
        let mut store = CommitStore::new(db_dir, config);
        store.load_version(0).unwrap();

        for i in 1u8..=5 {
            let cs = make_evm_changeset(vec![(storage_key(i, i), vec![i; 4], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        store.write_snapshot().unwrap();
        hash_at_v5 = store.root_hash();

        // Commit versions 6-10.
        for i in 6u8..=10 {
            let cs = make_evm_changeset(vec![(storage_key(i, i), vec![i; 4], false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 10);

        // Rollback to version 5.
        store.rollback(5).unwrap();
        assert_eq!(store.version(), 5);
        assert_eq!(store.root_hash(), hash_at_v5);

        // Data at version 5 should be present.
        let (val, found) = store.get(&storage_key(5, 5));
        assert!(found);
        assert_eq!(val.unwrap(), vec![5u8; 4]);

        // Data at version 6 should NOT be readable via get (pending writes cleared).
        // The key was committed before rollback, so after rollback the working dir
        // is restored from the v5 snapshot. Version 6 data should be gone.
        let (val6, found6) = store.get(&storage_key(6, 6));
        assert!(!found6, "version 6 data should be gone after rollback");
        assert!(val6.is_none());

        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 3. Persistence
// ---------------------------------------------------------------------------

#[test]
fn test_persistence_all_key_types() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    {
        let mut store = new_store(db_dir);

        // Nonce
        let cs_nonce = make_evm_changeset(vec![(nonce_key(0x01), encode_nonce(42), false)]);
        store.apply_change_sets(&cs_nonce).unwrap();

        // CodeHash (non-zero = contract)
        let ch = [0xAB_u8; 32];
        let cs_codehash = make_evm_changeset(vec![(codehash_key(0x02), ch.to_vec(), false)]);
        store.apply_change_sets(&cs_codehash).unwrap();

        // Code
        let cs_code = make_evm_changeset(vec![(code_key(0x03), b"bytecode_here".to_vec(), false)]);
        store.apply_change_sets(&cs_code).unwrap();

        // Storage
        let cs_storage =
            make_evm_changeset(vec![(storage_key(0x04, 0x01), b"slot_val".to_vec(), false)]);
        store.apply_change_sets(&cs_storage).unwrap();

        // Legacy
        let cs_legacy =
            make_evm_changeset(vec![(legacy_key(0x05), b"legacy_data".to_vec(), false)]);
        store.apply_change_sets(&cs_legacy).unwrap();

        store.commit().unwrap();
        store.close().unwrap();
    }

    // Reopen and verify all key types.
    {
        let mut store = new_store(db_dir);
        assert_eq!(store.version(), 1);

        let (val, found) = store.get(&nonce_key(0x01));
        assert!(found);
        assert_eq!(u64::from_be_bytes(val.unwrap().try_into().unwrap()), 42);

        let (val, found) = store.get(&codehash_key(0x02));
        assert!(found);
        assert_eq!(val.unwrap(), [0xAB_u8; 32].to_vec());

        let (val, found) = store.get(&code_key(0x03));
        assert!(found);
        assert_eq!(val.unwrap(), b"bytecode_here");

        let (val, found) = store.get(&storage_key(0x04, 0x01));
        assert!(found);
        assert_eq!(val.unwrap(), b"slot_val");

        let (val, found) = store.get(&legacy_key(0x05));
        assert!(found);
        assert_eq!(val.unwrap(), b"legacy_data");

        store.close().unwrap();
    }
}

#[test]
fn test_lt_hash_deterministic_across_reopen() {
    let dir1 = tempdir().unwrap();
    let dir2 = tempdir().unwrap();

    let operations: Vec<(Vec<u8>, Vec<u8>)> =
        (1u8..=5).map(|i| (storage_key(i, i), vec![i; 8])).collect();

    let mut hashes = Vec::new();
    for db_dir in [dir1.path().to_str().unwrap(), dir2.path().to_str().unwrap()] {
        let mut store = new_store(db_dir);
        for (key, val) in &operations {
            let cs = make_evm_changeset(vec![(key.clone(), val.clone(), false)]);
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        hashes.push(store.root_hash());
        store.close().unwrap();
    }

    assert_eq!(hashes[0], hashes[1], "same operations should produce same LtHash");
}

// ---------------------------------------------------------------------------
// 4. Mixed operations
// ---------------------------------------------------------------------------

#[test]
fn test_overwrite_same_key_single_block() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());
    let key = storage_key(0x10, 0x20);

    // Write key twice in one block; second value wins.
    let cs = make_evm_changeset(vec![
        (key.clone(), b"first".to_vec(), false),
        (key.clone(), b"second".to_vec(), false),
    ]);
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();

    let (val, found) = store.get(&key);
    assert!(found);
    assert_eq!(val.unwrap(), b"second");

    store.close().unwrap();
}

#[test]
fn test_delete_then_rewrite() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());
    let key = storage_key(0x20, 0x30);

    // First commit: write.
    let cs1 = make_evm_changeset(vec![(key.clone(), b"original".to_vec(), false)]);
    store.apply_change_sets(&cs1).unwrap();
    store.commit().unwrap();

    // Second block: delete then set in same changeset.
    let cs2 = make_evm_changeset(vec![
        (key.clone(), vec![], true),
        (key.clone(), b"resurrected".to_vec(), false),
    ]);
    store.apply_change_sets(&cs2).unwrap();
    store.commit().unwrap();

    let (val, found) = store.get(&key);
    assert!(found);
    assert_eq!(val.unwrap(), b"resurrected");

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 5. Import
// ---------------------------------------------------------------------------

#[test]
fn test_import_basic() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    {
        let mut store = new_store(db_dir);

        {
            let mut importer = KvImporter::new(&mut store, 10);
            for i in 0..5u8 {
                let key = storage_key(i, i);
                let value = format!("imported_{i}");
                importer.add_node(&key, value.as_bytes(), 0).unwrap();
            }
            importer.close().unwrap();
        }

        assert_eq!(store.version(), 10);

        // Data should be readable via get.
        for i in 0..5u8 {
            let (val, found) = store.get(&storage_key(i, i));
            assert!(found, "imported key {i} should be found");
            assert_eq!(val.unwrap(), format!("imported_{i}").as_bytes());
        }

        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 6. Iterator
// ---------------------------------------------------------------------------

#[test]
fn test_iterator_after_commit() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    // Commit several storage entries.
    for i in 1u8..=3 {
        let cs =
            make_evm_changeset(vec![(storage_key(i, i), format!("val_{i}").into_bytes(), false)]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();
    }

    // Iterator over all committed data; scope the iterator so it is
    // dropped before the store is closed.
    {
        let mut iter = store.iterator(b"", b"");
        let mut count = 0;
        while iter.valid() {
            assert!(!iter.key().is_empty());
            assert!(!iter.value().is_empty());
            count += 1;
            iter.next();
        }
        iter.close().unwrap();

        assert!(count >= 3, "iterator should yield at least 3 entries, got {count}");
    }

    store.close().unwrap();
}
