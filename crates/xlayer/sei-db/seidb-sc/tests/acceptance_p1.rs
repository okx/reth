//! P1 acceptance tests for seidb-sc (B-01 through B-05).
//!
//! Ported from the Go reference tests in sei-db/state_db/sc/.

use seidb_common::{
    config::{FlatKvConfig, MemIavlConfig},
    evm_keys::STATE_KEY_PREFIX,
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet, TreeNameUpgrade};
use seidb_sc::{
    flatkv::store::CommitStore,
    memiavl::{db::DB, tree::Tree},
};
use std::fs;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers (shared with acceptance_p0)
// ---------------------------------------------------------------------------

fn new_store(dir: &str) -> CommitStore {
    let mut store = CommitStore::new(dir, FlatKvConfig::default());
    store.load_version(0).unwrap();
    store
}

fn make_evm_cs(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
    vec![NamedChangeSet { name: "evm".to_string(), changeset: Some(ChangeSet { pairs }) }]
}

fn kv(key: Vec<u8>, value: Vec<u8>) -> KvPair {
    KvPair { delete: false, key, value }
}

fn storage_key(addr_seed: u8, slot_seed: u8) -> Vec<u8> {
    let mut k = vec![STATE_KEY_PREFIX];
    let mut addr = [0u8; 20];
    addr[0] = addr_seed;
    let mut slot = [0u8; 32];
    slot[0] = slot_seed;
    k.extend_from_slice(&addr);
    k.extend_from_slice(&slot);
    k
}

fn commit_storage_entry(store: &mut CommitStore, addr_seed: u8, slot_seed: u8, value: u8) -> i64 {
    let sk = storage_key(addr_seed, slot_seed);
    let cs = make_evm_cs(vec![kv(sk, vec![value])]);
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap()
}

// ===========================================================================
// B-01: Partial snapshot cleanup
// ===========================================================================
// Ported from Go TestPartialSnapshotCleanup.
//
// Open CommitStore, commit a few versions, take a valid snapshot, then
// manually create a `-tmp` directory simulating an interrupted snapshot.
// Reopen — the `-tmp` directory should be cleaned up, data should be intact,
// and new commits should work.

#[test]
fn b01_partial_snapshot_cleanup() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    // Phase 1: create store, commit v1, take valid snapshot
    {
        let mut store = new_store(db_dir);
        commit_storage_entry(&mut store, 0x50, 0x01, 0x01);
        store.write_snapshot().unwrap();

        // Commit v2
        commit_storage_entry(&mut store, 0x50, 0x02, 0x02);

        store.close().unwrap();
    }

    // Sabotage: create a `-tmp` directory in the flatkv snapshot area
    // simulating an interrupted snapshot write for version 2.
    let flatkv_dir = std::path::Path::new(db_dir).join("flatkv");
    let tmp_path = flatkv_dir.join("snapshot-00000000000000000002-tmp");
    fs::create_dir_all(&tmp_path).unwrap();
    assert!(tmp_path.exists(), "tmp dir should exist before reopen");

    // Phase 2: reopen — tmp dir should be cleaned up
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        store.load_version(0).unwrap();

        // tmp dir should be cleaned up
        assert!(!tmp_path.exists(), "tmp dir should be cleaned up on reopen");

        // Data should be intact (v1 + v2)
        assert_eq!(store.version(), 2);

        let (val, found) = store.get(&storage_key(0x50, 0x01));
        assert!(found, "v1 data should be intact");
        assert_eq!(val.unwrap(), vec![0x01]);

        let (val, found) = store.get(&storage_key(0x50, 0x02));
        assert!(found, "v2 data should be intact");
        assert_eq!(val.unwrap(), vec![0x02]);

        // New commits should work
        commit_storage_entry(&mut store, 0x50, 0x03, 0x03);
        assert_eq!(store.version(), 3);

        let (val, found) = store.get(&storage_key(0x50, 0x03));
        assert!(found, "new commit should work after cleanup");
        assert_eq!(val.unwrap(), vec![0x03]);

        store.close().unwrap();
    }

    // Phase 3: final reopen to ensure everything is consistent.
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        store.load_version(0).unwrap();
        assert_eq!(store.version(), 3, "should be at version 3 after final reopen");
        store.close().unwrap();
    }
}

// ===========================================================================
// B-02: Orphan snapshot recovery
// ===========================================================================
// Ported from Go TestOrphanSnapshotRecovery.
//
// Create a snapshot directory (simulating it exists on disk), delete the
// `current` symlink (orphaning the snapshot). Reopen — the store should
// detect the orphaned snapshot directory and recover by re-creating the
// symlink.

#[test]
fn b02_orphan_snapshot_recovery() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();
    let flatkv_dir = std::path::Path::new(db_dir).join("flatkv");

    // Create a snapshot directory for version 5 with all required sub-dirs.
    let snap_name = "snapshot-00000000000000000005";
    let snap_dir = flatkv_dir.join(snap_name);
    let sub_dirs = ["account", "code", "storage", "legacy", "metadata"];
    for sub in &sub_dirs {
        fs::create_dir_all(snap_dir.join(sub)).unwrap();
    }

    // Verify no current symlink exists.
    let current_link = flatkv_dir.join("current");
    assert!(
        !current_link.exists() && fs::read_link(&current_link).is_err(),
        "no current symlink should exist"
    );

    // Open the store — should detect orphan and recover.
    let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
    store.load_version(0).unwrap();

    // current symlink should now exist and point to snapshot-5.
    let target = fs::read_link(&current_link).unwrap();
    assert_eq!(
        target.to_str().unwrap(),
        snap_name,
        "symlink should be recovered to orphan snapshot"
    );

    store.close().unwrap();
}

// ===========================================================================
// B-03: WAL offset not found
// ===========================================================================
// Ported from Go TestWalOffsetForVersionNotFound.
//
// Open CommitStore, commit 2 versions. Try to look up a WAL offset for a
// version that doesn't exist (version 10). The observable behavior: loading
// that version should fail because the WAL doesn't contain it.

#[test]
fn b03_wal_offset_for_version_not_found() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    // Phase 1: commit 2 versions and snapshot, then close.
    {
        let mut store = new_store(db_dir);
        commit_storage_entry(&mut store, 0x01, 0x01, 0x01);
        commit_storage_entry(&mut store, 0x02, 0x02, 0x02);
        store.write_snapshot().unwrap();
        store.close().unwrap();
    }

    // Phase 2: try to load version 10 (well beyond WAL) — should fail.
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        let result = store.load_version(10);
        assert!(result.is_err(), "loading version 10 should fail when WAL only has 2 entries");
    }

    // Phase 3: load version 0 (latest) should still work fine.
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        store.load_version(0).unwrap();
        assert_eq!(store.version(), 2);

        let (val, found) = store.get(&storage_key(0x01, 0x01));
        assert!(found);
        assert_eq!(val.unwrap(), vec![0x01]);

        store.close().unwrap();
    }
}

// ===========================================================================
// B-04: Snapshot writer resource cleanup on drop
// ===========================================================================
// Test: Create a MemIAVL tree with 1000 nodes. Write a snapshot to a
// temp directory. Drop the tree. Verify no tmp files leaked, the snapshot
// files exist, and a new tree can be loaded from the snapshot.

#[test]
fn b04_snapshot_writer_resource_cleanup_on_drop() {
    let dir = tempdir().unwrap();
    let snap_dir = dir.path().join("snapshot_test");

    // Create a tree with 1000 nodes.
    let mut tree = Tree::new_empty(0, 0);
    for i in 0u32..1000 {
        let key = format!("key_{i:06}");
        let value = format!("value_{i:06}");
        tree.set(key.as_bytes(), value.as_bytes());
    }
    tree.save_version(true).unwrap();

    // Write snapshot.
    tree.write_snapshot(&snap_dir).unwrap();

    // Verify snapshot files exist.
    assert!(snap_dir.join("nodes").exists(), "nodes file should exist");
    assert!(snap_dir.join("leaves").exists(), "leaves file should exist");
    assert!(snap_dir.join("kvs").exists(), "kvs file should exist");
    assert!(snap_dir.join("metadata").exists(), "metadata file should exist");

    // Drop the tree — should not leak handles.
    let root_hash_before = tree.root_hash();
    drop(tree);

    // Verify no tmp files remain in the snapshot directory.
    for entry in fs::read_dir(&snap_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(!name.contains("-tmp"), "no tmp files should remain: {name}");
    }

    // Verify the tree is still loadable from snapshot.
    let loaded_snapshot = seidb_sc::memiavl::snapshot::Snapshot::open(&snap_dir).unwrap();
    let loaded_tree = Tree::new_from_snapshot(loaded_snapshot);
    assert_eq!(loaded_tree.root_hash(), root_hash_before, "hash should match after reload");

    // Spot-check a few keys.
    let val = loaded_tree.get(b"key_000000");
    assert_eq!(val, Some(b"value_000000".to_vec()), "first key should be readable");
    let val = loaded_tree.get(b"key_000999");
    assert_eq!(val, Some(b"value_000999".to_vec()), "last key should be readable");
}

// ===========================================================================
// B-05: Close waits for background snapshot
// ===========================================================================
// Ported from Go TestCloseWaitsForBackgroundSnapshot.
//
// Open MemIAVL DB with snapshot_interval=1 to trigger background snapshot
// on every commit. Commit data, then immediately close. Verify close()
// completes without panic and the data is recoverable on reopen.

#[test]
fn b05_close_waits_for_background_snapshot() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path();

    // Phase 1: open DB, commit, close (background snapshot may be triggered)
    {
        let config = MemIavlConfig {
            snapshot_interval: 1, // trigger on every commit
            ..Default::default()
        };
        let mut db = DB::open(db_dir, 0, &config, false).unwrap();

        // Create the "test" tree.
        db.apply_upgrades(&[TreeNameUpgrade {
            name: "test".to_string(),
            rename_from: String::new(),
            delete: false,
        }])
        .unwrap();

        // Apply and commit data.
        let cs = vec![NamedChangeSet {
            name: "test".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: b"key1".to_vec(),
                    value: b"value1".to_vec(),
                }],
            }),
        }];
        db.apply_change_sets(&cs).unwrap();
        let v = db.commit().unwrap();
        assert_eq!(v, 1);

        // Close should wait for background snapshot and not panic.
        db.close().unwrap();
    }

    // Phase 2: reopen and verify data is intact.
    {
        let config = MemIavlConfig::default();
        let db = DB::open(db_dir, 0, &config, true).unwrap();
        assert!(db.version() >= 1, "version should be at least 1 after reopen");

        let tree = db.tree_by_name("test");
        assert!(tree.is_some(), "test tree should exist");
        let val = tree.unwrap().get(b"key1");
        assert_eq!(val, Some(b"value1".to_vec()), "data should survive close");
    }
}
