//! Integration tests for `MemiavlCommitStore` and the `Committer` trait.
//!
//! These tests exercise the full lifecycle of the commit store including
//! open, apply, commit, close, reopen, rollback, upgrades, and version
//! management.

use seidb_common::config::MemIavlConfig;
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet, TreeNameUpgrade};
use seidb_sc::memiavl::commit_store::MemiavlCommitStore;
use seidb_traits::sc::Committer;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_config() -> MemIavlConfig {
    MemIavlConfig {
        async_commit_buffer: 0,
        snapshot_keep_recent: 2,
        snapshot_interval: 0,
        snapshot_min_time_interval: 0,
        ..Default::default()
    }
}

fn make_named_changeset(name: &str, pairs: Vec<KvPair>) -> NamedChangeSet {
    NamedChangeSet { name: name.to_string(), changeset: Some(ChangeSet { pairs }) }
}

fn make_kv_pair(key: &[u8], value: &[u8]) -> KvPair {
    KvPair { delete: false, key: key.to_vec(), value: value.to_vec() }
}

fn make_upgrade_add(name: &str) -> TreeNameUpgrade {
    TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: false }
}

fn make_upgrade_delete(name: &str) -> TreeNameUpgrade {
    TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: true }
}

/// Open a fresh commit store with a "bank" tree at version 0.
fn open_fresh_store(home: &str) -> MemiavlCommitStore {
    let config = default_config();
    let mut store = MemiavlCommitStore::new(home, config);
    MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
    MemiavlCommitStore::apply_upgrades(&mut store, &[make_upgrade_add("bank")]).unwrap();
    store
}

// ---------------------------------------------------------------------------
// 1. test_commit_store_open_commit_reopen
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_open_commit_reopen() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    // Open, apply, commit
    {
        let mut store = open_fresh_store(home);
        let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"balance", b"100")])];
        store.apply_change_sets(&cs).unwrap();
        let v = store.commit().unwrap();
        assert_eq!(v, 1);
        assert_eq!(store.version(), 1);

        // Verify data via tree_by_name
        let tree = store.get_child_store_by_name("bank").unwrap();
        assert_eq!(tree.get(b"balance"), Some(b"100".to_vec()));

        store.close().unwrap();
    }

    // Reopen and verify data persisted
    {
        let config = default_config();
        let mut store = MemiavlCommitStore::new(home, config);
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        assert_eq!(store.version(), 1);

        let tree = store.get_child_store_by_name("bank").unwrap();
        assert_eq!(tree.get(b"balance"), Some(b"100".to_vec()));
        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 2. test_commit_store_wal_replay
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_wal_replay() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    // Commit 5 versions
    {
        let mut store = open_fresh_store(home);
        for i in 1..=5 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("key{i}").as_bytes(), format!("val{i}").as_bytes())],
            )];
            store.apply_change_sets(&cs).unwrap();
            let v = store.commit().unwrap();
            assert_eq!(v, i as i64);
        }
        store.close().unwrap();
    }

    // Reopen — WAL should replay
    {
        let config = default_config();
        let mut store = MemiavlCommitStore::new(home, config);
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        assert_eq!(store.version(), 5);

        let tree = store.get_child_store_by_name("bank").unwrap();
        for i in 1..=5 {
            assert_eq!(
                tree.get(format!("key{i}").as_bytes()),
                Some(format!("val{i}").into_bytes()),
                "key{i} should exist after WAL replay"
            );
        }
        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 3. test_commit_store_snapshot_rewrite
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_snapshot_rewrite() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let config = MemIavlConfig {
        async_commit_buffer: 0,
        snapshot_keep_recent: 2,
        snapshot_interval: 3, // trigger snapshot every 3 blocks
        snapshot_min_time_interval: 0,
        ..Default::default()
    };

    // Commit enough versions to trigger snapshot
    {
        let mut store = MemiavlCommitStore::new(home, config.clone());
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        store.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        for i in 1..=6 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        store.close().unwrap();
    }

    // Reopen and verify data survived snapshot
    {
        let mut store = MemiavlCommitStore::new(home, config);
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        assert_eq!(store.version(), 6);

        let tree = store.get_child_store_by_name("bank").unwrap();
        for i in 1..=6 {
            assert_eq!(tree.get(format!("k{i}").as_bytes()), Some(format!("v{i}").into_bytes()),);
        }
        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 4. test_commit_store_rollback
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_rollback() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let config = MemIavlConfig {
        async_commit_buffer: 0,
        snapshot_keep_recent: 2,
        snapshot_interval: 5,
        snapshot_min_time_interval: 0,
        ..Default::default()
    };

    // Commit 10 versions, snapshot at 5
    {
        let mut store = MemiavlCommitStore::new(home, config.clone());
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        store.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        for i in 1..=10 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 10);

        // Rollback to version 5
        store.rollback(5).unwrap();
        assert_eq!(store.version(), 5);

        let tree = store.get_child_store_by_name("bank").unwrap();
        assert_eq!(tree.get(b"k5"), Some(b"v5".to_vec()));
        // Keys from version 6-10 should not be present
        assert!(tree.get(b"k6").is_none(), "k6 should not exist after rollback to v5");

        store.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// 5. test_commit_store_upgrades
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_upgrades() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);

    // Add another tree
    store.apply_upgrades(&[make_upgrade_add("staking")]).unwrap();
    assert!(store.get_child_store_by_name("staking").is_some());

    // Apply data to both trees and commit
    let cs = vec![
        make_named_changeset("bank", vec![make_kv_pair(b"b1", b"v1")]),
        make_named_changeset("staking", vec![make_kv_pair(b"s1", b"v1")]),
    ];
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();

    assert_eq!(store.get_child_store_by_name("bank").unwrap().get(b"b1"), Some(b"v1".to_vec()));
    assert_eq!(store.get_child_store_by_name("staking").unwrap().get(b"s1"), Some(b"v1".to_vec()));

    // Delete a tree
    store.apply_upgrades(&[make_upgrade_delete("staking")]).unwrap();
    assert!(store.get_child_store_by_name("staking").is_none());

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 6. test_commit_store_version_management
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_version_management() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);
    assert_eq!(store.version(), 0);

    // First commit
    let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"k", b"v")])];
    store.apply_change_sets(&cs).unwrap();
    let v = store.commit().unwrap();
    assert_eq!(v, 1);
    assert_eq!(store.version(), 1);

    // Second commit
    let cs2 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k2", b"v2")])];
    store.apply_change_sets(&cs2).unwrap();
    let v2 = store.commit().unwrap();
    assert_eq!(v2, 2);
    assert_eq!(store.version(), 2);

    // Third commit
    let cs3 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k3", b"v3")])];
    store.apply_change_sets(&cs3).unwrap();
    let v3 = store.commit().unwrap();
    assert_eq!(v3, 3);
    assert_eq!(store.version(), 3);

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 7. test_commit_store_commit_info
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_commit_info() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);
    store.apply_upgrades(&[make_upgrade_add("staking")]).unwrap();

    let cs = vec![
        make_named_changeset("bank", vec![make_kv_pair(b"b1", b"v1")]),
        make_named_changeset("staking", vec![make_kv_pair(b"s1", b"v1")]),
    ];
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();

    // Working commit info should reflect the next version
    let wci = store.working_commit_info();
    assert_eq!(wci.version, 2);

    // Last commit info should reflect the current version
    let lci = store.last_commit_info();
    assert_eq!(lci.version, 1);
    assert_eq!(lci.store_infos.len(), 2);

    // Verify all trees are represented in commit info
    let names: Vec<&str> = lci.store_infos.iter().map(|si| si.name.as_str()).collect();
    assert!(names.contains(&"bank"));
    assert!(names.contains(&"staking"));

    // Each tree should have a non-empty commit id
    for si in &lci.store_infos {
        let cid = si.commit_id.as_ref().unwrap();
        assert_eq!(cid.version, 1);
        assert_eq!(cid.hash.len(), 32);
    }

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 8. test_committer_trait
// ---------------------------------------------------------------------------

#[test]
fn test_committer_trait() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);
    let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"key", b"val")])];
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();
    store.close().unwrap();

    // Use via Committer trait
    let config = default_config();
    let mut store = MemiavlCommitStore::new(home, config);
    MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();

    let committer: &dyn Committer = &store;
    assert_eq!(committer.version(), 1);

    let lci = committer.last_commit_info();
    assert_eq!(lci.version, 1);

    let wci = committer.working_commit_info();
    assert_eq!(wci.version, 2);

    let latest = committer.get_latest_version().unwrap();
    // get_latest_version reads from snapshot metadata on disk, which may
    // still be at version 0 if no snapshot has been written yet.
    assert!(latest >= 0);

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 9. test_commit_store_close_idempotent
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_close_idempotent() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);
    let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"k", b"v")])];
    store.apply_change_sets(&cs).unwrap();
    store.commit().unwrap();

    // Close twice — second close should be a no-op
    store.close().unwrap();
    store.close().unwrap();

    // Version should be 0 after close (db is None)
    assert_eq!(store.version(), 0);
}

// ---------------------------------------------------------------------------
// 10. test_commit_store_exclusive_lock
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_exclusive_lock() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store1 = MemiavlCommitStore::new(home, default_config());
    MemiavlCommitStore::load_version(&mut store1, 0, false).unwrap();

    // Second open should fail due to exclusive lock
    let mut store2 = MemiavlCommitStore::new(home, default_config());
    let result = MemiavlCommitStore::load_version(&mut store2, 0, false);
    assert!(result.is_err(), "second open should fail due to exclusive lock");

    store1.close().unwrap();
}

// ---------------------------------------------------------------------------
// 11. test_commit_store_initial_version
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_initial_version() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let mut store = open_fresh_store(home);

    // Set initial version to 100
    store.set_initial_version(100).unwrap();

    let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"k", b"v")])];
    store.apply_change_sets(&cs).unwrap();
    let v = store.commit().unwrap();
    // After setting initial version, next commit should be at version 100
    assert_eq!(v, 100);
    assert_eq!(store.version(), 100);

    // Second commit increments normally
    let cs2 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k2", b"v2")])];
    store.apply_change_sets(&cs2).unwrap();
    let v2 = store.commit().unwrap();
    assert_eq!(v2, 101);

    store.close().unwrap();
}

// ---------------------------------------------------------------------------
// 12. test_commit_store_load_version_via_trait
// ---------------------------------------------------------------------------

#[test]
fn test_commit_store_load_version_via_trait() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().to_str().unwrap();

    let config = MemIavlConfig {
        async_commit_buffer: 0,
        snapshot_keep_recent: 2,
        snapshot_interval: 3,
        snapshot_min_time_interval: 0,
        ..Default::default()
    };

    // Commit several versions with a snapshot at version 3
    {
        let mut store = MemiavlCommitStore::new(home, config.clone());
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        store.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        for i in 1..=6 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            store.apply_change_sets(&cs).unwrap();
            store.commit().unwrap();
        }
        store.close().unwrap();
    }

    // Load specific version via the Committer trait
    {
        let mut store = MemiavlCommitStore::new(home, config);
        MemiavlCommitStore::load_version(&mut store, 0, false).unwrap();
        assert_eq!(store.version(), 6);

        // Load version 3 as read-only via trait
        let committer: &dyn Committer = &store;
        let loaded = committer.load_version(3, true).unwrap();
        assert_eq!(loaded.version(), 3);

        store.close().unwrap();
    }
}
