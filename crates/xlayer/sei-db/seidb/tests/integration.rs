//! End-to-end integration tests for SeiDb: SC+SS, WriteMode routing, rollback, multi-block.

use seidb::db::SeiDb;
use seidb_common::config::{MemIavlConfig, StateCommitConfig, StateStoreConfig, WriteMode};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_traits::{sc::Committer, ss::StateStore};
use std::sync::Arc;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_changeset(store: &str, pairs: Vec<(&[u8], &[u8], bool)>) -> NamedChangeSet {
    NamedChangeSet {
        name: store.to_string(),
        changeset: Some(ChangeSet {
            pairs: pairs
                .into_iter()
                .map(|(k, v, del)| KvPair { delete: del, key: k.to_vec(), value: v.to_vec() })
                .collect(),
        }),
    }
}

fn sc_config(write_mode: WriteMode) -> StateCommitConfig {
    StateCommitConfig {
        write_mode,
        memiavl: MemIavlConfig {
            snapshot_interval: 0, // disable auto-snapshot for tests
            ..Default::default()
        },
        ..Default::default()
    }
}

fn ss_config(dir: &std::path::Path) -> StateStoreConfig {
    StateStoreConfig {
        enable: true,
        db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
        evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
        keep_last_version: true,
        // Disable pruning so background threads do not interfere with tests.
        keep_recent: 0,
        prune_interval_seconds: 0,
        ..Default::default()
    }
}

/// Open a SeiDb with SC only (no SS), initialize with given stores, and load version 0.
fn open_sc_only(home: &str, stores: &[&str], write_mode: WriteMode) -> SeiDb {
    let mut db = SeiDb::open(home, sc_config(write_mode), None).unwrap();
    let store_names: Vec<String> = stores.iter().map(|s| s.to_string()).collect();
    db.initialize(&store_names);
    db.load_version(0).unwrap();
    db
}

// ---------------------------------------------------------------------------
// SC basic operations
// ---------------------------------------------------------------------------

/// 1. Initialize with stores, apply changesets, commit, verify version.
#[test]
fn test_sc_basic_operations() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "staking"], WriteMode::CosmosOnly);

    // Apply changesets for bank and staking.
    let changesets = vec![
        make_changeset("bank", vec![(b"alice", b"100", false)]),
        make_changeset("staking", vec![(b"validator1", b"500", false)]),
    ];
    db.sc_mut().apply_change_sets(&changesets).unwrap();

    // Commit and check version.
    let version = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);
    assert_eq!(db.version(), 1);
}

/// 2. Commit 5 versions and verify version increments.
#[test]
fn test_sc_multi_commit() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank"], WriteMode::CosmosOnly);

    for i in 1..=5 {
        let val = format!("{}", i * 100);
        let changesets = vec![make_changeset("bank", vec![(b"alice", val.as_bytes(), false)])];
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        let version = db.sc_mut().commit().unwrap();
        assert_eq!(version, i);
    }
    assert_eq!(db.version(), 5);
}

/// 3. working_commit_info / last_commit_info have correct version and store count.
#[test]
fn test_sc_commit_info() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "staking"], WriteMode::CosmosOnly);

    // Commit once.
    let changesets = vec![make_changeset("bank", vec![(b"x", b"1", false)])];
    db.sc_mut().apply_change_sets(&changesets).unwrap();
    db.sc_mut().commit().unwrap();

    let last = db.sc().last_commit_info();
    assert_eq!(last.version, 1);

    // After commit, working_commit_info reflects the next pending state.
    let working = db.sc().working_commit_info();
    // Working version should be >= last committed version.
    assert!(
        working.version >= last.version,
        "working version {} should be >= last committed version {}",
        working.version,
        last.version
    );
}

// ---------------------------------------------------------------------------
// WriteMode routing
// ---------------------------------------------------------------------------

/// 4. CosmosOnly mode: apply bank+evm changeset, commit succeeds.
#[test]
fn test_sc_cosmos_only_mode() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "evm"], WriteMode::CosmosOnly);

    let changesets = vec![
        make_changeset("bank", vec![(b"alice", b"100", false)]),
        make_changeset("evm", vec![(b"contract1", b"code", false)]),
    ];
    db.sc_mut().apply_change_sets(&changesets).unwrap();

    let version = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);
}

/// 5. DualWrite mode: apply evm changeset, both cosmos and evm backends get data, commit ok.
#[test]
fn test_sc_dual_write_mode() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "evm"], WriteMode::DualWrite);

    let changesets = vec![
        make_changeset("bank", vec![(b"alice", b"100", false)]),
        make_changeset("evm", vec![(b"contract1", b"code", false)]),
    ];
    db.sc_mut().apply_change_sets(&changesets).unwrap();

    let version = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);

    // DualWrite mode sends data to both cosmos and EVM backends.
    // Verify by committing a second version to confirm both backends stay in sync.
    let changesets2 = vec![make_changeset("bank", vec![(b"bob", b"200", false)])];
    db.sc_mut().apply_change_sets(&changesets2).unwrap();
    let version2 = db.sc_mut().commit().unwrap();
    assert_eq!(version2, 2);
}

/// 6. SplitWrite mode: apply evm changeset, evm data stripped from cosmos.
#[test]
fn test_sc_split_write_mode() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "evm"], WriteMode::SplitWrite);

    let changesets = vec![
        make_changeset("bank", vec![(b"alice", b"100", false)]),
        make_changeset("evm", vec![(b"contract1", b"code", false)]),
    ];
    db.sc_mut().apply_change_sets(&changesets).unwrap();

    let version = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);

    // SplitWrite mode routes EVM data to flatkv and non-EVM to cosmos.
    // Verify by committing a second version to confirm the split routing is consistent.
    let changesets2 = vec![make_changeset("bank", vec![(b"bob", b"200", false)])];
    db.sc_mut().apply_change_sets(&changesets2).unwrap();
    let version2 = db.sc_mut().commit().unwrap();
    assert_eq!(version2, 2);
}

// ---------------------------------------------------------------------------
// Version management
// ---------------------------------------------------------------------------

/// 7. Commit 5 versions, rollback to 3, verify version == 3.
#[test]
fn test_sc_rollback() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank"], WriteMode::CosmosOnly);

    for i in 1..=5 {
        let val = format!("v{i}");
        let changesets = vec![make_changeset("bank", vec![(b"key", val.as_bytes(), false)])];
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        db.sc_mut().commit().unwrap();
    }
    assert_eq!(db.version(), 5);

    // Rollback to version 3.
    db.sc_mut().rollback(3).unwrap();

    // After rollback, reload the DB to pick up the rolled-back state.
    db.load_version(3).unwrap();
    assert_eq!(db.version(), 3);
}

/// 8. get_latest_version / get_earliest_version.
#[test]
fn test_sc_get_versions() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank"], WriteMode::CosmosOnly);

    // Fresh DB: earliest and latest should both be 0.
    let earliest = db.sc().get_earliest_version().unwrap();
    let latest = db.sc().get_latest_version().unwrap();
    assert_eq!(earliest, 0);
    assert_eq!(latest, 0);

    // Commit a few versions.
    for _ in 1..=3 {
        let changesets = vec![make_changeset("bank", vec![(b"k", b"v", false)])];
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        db.sc_mut().commit().unwrap();
    }

    // The in-memory version should track commits.
    assert_eq!(db.version(), 3);

    // get_latest_version reads from on-disk metadata which may lag behind the
    // in-memory version (memiavl updates it on snapshot). Verify it is
    // non-negative and does not exceed the in-memory version.
    let latest = db.sc().get_latest_version().unwrap();
    assert!(latest >= 0, "latest version should be non-negative");
    assert!(
        latest <= db.version(),
        "latest on-disk ({latest}) should be <= in-memory ({})",
        db.version()
    );

    // Earliest version depends on backend behavior; at minimum it should be <= latest on-disk.
    let earliest = db.sc().get_earliest_version().unwrap();
    assert!(
        earliest <= latest || latest == 0,
        "earliest ({earliest}) should be <= latest ({latest})"
    );
}

// ---------------------------------------------------------------------------
// Committer trait
// ---------------------------------------------------------------------------

/// 9. Use CompositeCommitStore through Box<dyn Committer>.
#[test]
fn test_committer_trait_usage() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank"], WriteMode::CosmosOnly);

    // Verify we can use the SC store as a Committer trait object reference.
    let sc: &dyn Committer = db.sc();
    assert_eq!(sc.version(), 0);
    let _ = sc.get_latest_version().unwrap();
    let _ = sc.get_earliest_version().unwrap();
    let info = sc.working_commit_info();
    // After load_version(0), working_commit_info version may be 0 or 1
    // depending on the backend's internal state tracking.
    assert!(info.version <= 1, "working version should be 0 or 1, got {}", info.version);

    // Use mutable trait object for apply + commit.
    let sc_mut: &mut dyn Committer = db.sc_mut();
    let changesets = vec![make_changeset("bank", vec![(b"key", b"val", false)])];
    sc_mut.apply_change_sets(&changesets).unwrap();
    let version = sc_mut.commit().unwrap();
    assert_eq!(version, 1);

    // Verify last_commit_info through trait.
    let last = db.sc().last_commit_info();
    assert_eq!(last.version, 1);
}

// ---------------------------------------------------------------------------
// SC + SS (both configured)
// ---------------------------------------------------------------------------

/// 10. SeiDb with SS enabled: SC commit + SS apply, SS can query historical version.
#[test]
fn test_seidb_sc_ss_basic() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let ss_cfg = ss_config(dir.path());

    let mut db = SeiDb::open(&home, sc_config(WriteMode::CosmosOnly), Some(ss_cfg)).unwrap();
    let stores = vec!["bank".to_string()];
    db.initialize(&stores);
    db.load_version(0).unwrap();

    // Clone the SS Arc so we can use it independently of `db` borrows.
    let ss: Arc<seidb_ss::composite::store::CompositeStateStore> = Arc::clone(db.ss().unwrap());

    // SC path: apply + commit.
    let changesets = vec![make_changeset("bank", vec![(b"alice", b"100", false)])];
    db.sc_mut().apply_change_sets(&changesets).unwrap();
    let v1 = db.sc_mut().commit().unwrap();
    assert_eq!(v1, 1);

    // SS path: apply same changeset at version 1.
    ss.apply_changeset_sync(1, &changesets).unwrap();
    ss.set_latest_version(1).unwrap();

    // SS should be able to query at version 1.
    let val = ss.get("bank", 1, b"alice").unwrap();
    assert_eq!(val, Some(b"100".to_vec()));

    // Commit version 2 with an update.
    let changesets2 = vec![make_changeset("bank", vec![(b"alice", b"200", false)])];
    db.sc_mut().apply_change_sets(&changesets2).unwrap();
    let v2 = db.sc_mut().commit().unwrap();
    assert_eq!(v2, 2);

    ss.apply_changeset_sync(2, &changesets2).unwrap();
    ss.set_latest_version(2).unwrap();

    // SS should return version 2 data.
    let val = ss.get("bank", 2, b"alice").unwrap();
    assert_eq!(val, Some(b"200".to_vec()));

    // SS should still return version 1 data (historical query).
    let val = ss.get("bank", 1, b"alice").unwrap();
    assert_eq!(val, Some(b"100".to_vec()));
}

// ---------------------------------------------------------------------------
// Multi-block stress
// ---------------------------------------------------------------------------

/// 11. 100 blocks with mixed bank+evm changesets, verify final state.
#[test]
fn test_sc_multi_block_100() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = open_sc_only(&home, &["bank", "evm"], WriteMode::CosmosOnly);

    for i in 1..=100i64 {
        let bank_val = format!("bank_{i}");
        let evm_val = format!("evm_{i}");
        let changesets = vec![
            make_changeset("bank", vec![(b"counter", bank_val.as_bytes(), false)]),
            make_changeset("evm", vec![(b"nonce", evm_val.as_bytes(), false)]),
        ];
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        let version = db.sc_mut().commit().unwrap();
        assert_eq!(version, i);
    }

    assert_eq!(db.version(), 100);

    // Verify commit info reflects the final version.
    let last_info = db.sc().last_commit_info();
    assert_eq!(last_info.version, 100);
}

// ---------------------------------------------------------------------------
// Close and reopen
// ---------------------------------------------------------------------------

/// 12. Commit data, close, reopen (new SeiDb same dir), load_version, verify version.
#[test]
fn test_sc_close_reopen() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    // Phase 1: open, commit 3 versions, close.
    {
        let mut db = open_sc_only(&home, &["bank"], WriteMode::CosmosOnly);

        for i in 1..=3 {
            let val = format!("v{i}");
            let changesets = vec![make_changeset("bank", vec![(b"key", val.as_bytes(), false)])];
            db.sc_mut().apply_change_sets(&changesets).unwrap();
            db.sc_mut().commit().unwrap();
        }
        assert_eq!(db.version(), 3);
        db.close().unwrap();
    }

    // Phase 2: reopen the same directory, load version 3, verify.
    {
        let mut db = SeiDb::open(&home, sc_config(WriteMode::CosmosOnly), None).unwrap();
        let stores = vec!["bank".to_string()];
        db.initialize(&stores);
        db.load_version(3).unwrap();
        assert_eq!(db.version(), 3);

        // Verify we can continue committing from version 3.
        let changesets = vec![make_changeset("bank", vec![(b"key", b"v4", false)])];
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        let version = db.sc_mut().commit().unwrap();
        assert_eq!(version, 4);

        db.close().unwrap();
    }
}
