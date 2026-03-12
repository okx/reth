//! Crash recovery tests for SeiDb.
//!
//! Simulates crash scenarios via filesystem manipulation to verify WAL replay
//! and snapshot recovery behave correctly on reopen.

use seidb::db::SeiDb;
use seidb_common::config::{MemIavlConfig, StateCommitConfig, WalConfig, WriteMode};
use seidb_proto::{ChangeSet, ChangelogEntry, KvPair, NamedChangeSet};
use seidb_traits::wal::Wal;
use seidb_wal::changelog::new_changelog_wal;
use std::fs;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sc_config() -> StateCommitConfig {
    StateCommitConfig {
        write_mode: WriteMode::CosmosOnly,
        memiavl: MemIavlConfig {
            snapshot_interval: 0, // disable auto-snapshot for deterministic tests
            ..Default::default()
        },
        ..Default::default()
    }
}

fn open_and_init(home: &str, stores: &[&str]) -> SeiDb {
    let mut db = SeiDb::open(home, sc_config(), None).unwrap();
    let store_names: Vec<String> = stores.iter().map(|s| s.to_string()).collect();
    db.initialize(&store_names);
    db.load_version(0).unwrap();
    db
}

fn commit_n_blocks(db: &mut SeiDb, n: u64) {
    for i in 1..=n {
        let val = format!("value_{i}");
        let cs = vec![NamedChangeSet {
            name: "bank".into(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: format!("key_{i}").into_bytes(),
                    value: val.into_bytes(),
                }],
            }),
        }];
        db.sc_mut().apply_change_sets(&cs).unwrap();
        let version = db.sc_mut().commit().unwrap();
        assert_eq!(version, i as i64);
    }
}

// ---------------------------------------------------------------------------
// Test 1: Crash after WAL write, before full commit cycle
// ---------------------------------------------------------------------------

/// Simulates a crash where an extra WAL entry (v11) was written to disk but
/// the memiavl in-memory state was never updated. On reopen, WAL replay
/// should catch up and include v11.
#[test]
fn test_crash_after_wal_write_before_commit() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    // Commit 10 blocks normally
    {
        let mut db = open_and_init(&home, &["bank"]);
        commit_n_blocks(&mut db, 10);
        assert_eq!(db.version(), 10);
        db.close().unwrap();
    }

    // Manually write an extra WAL entry (v11) directly to the changelog
    {
        let wal_path = dir.path().join("data").join("committer.db").join("changelog");
        let wal_config = WalConfig { fsync_enabled: false, ..Default::default() };
        let wal = new_changelog_wal(wal_config, &wal_path).unwrap();

        let entry = ChangelogEntry {
            version: 11,
            changesets: vec![NamedChangeSet {
                name: "bank".into(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: b"key_11".to_vec(),
                        value: b"value_11".to_vec(),
                    }],
                }),
            }],
            upgrades: vec![],
        };
        wal.write(entry).unwrap();
    }

    // Reopen: WAL replay should pick up v11
    {
        let mut db = SeiDb::open(&home, sc_config(), None).unwrap();
        db.initialize(&["bank".into()]);
        db.load_version(0).unwrap();
        assert_eq!(
            db.version(),
            11,
            "after WAL replay, version should be 11 but got {}",
            db.version()
        );
        db.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Test 2: Crash during snapshot (leftover -tmp directory)
// ---------------------------------------------------------------------------

/// Simulates an interrupted snapshot by creating a fake `-tmp` directory in
/// the memiavl snapshot area. On reopen, `remove_tmp_dirs` should clean it up
/// and the DB should load normally.
#[test]
fn test_crash_during_snapshot() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    // Commit 10 blocks normally
    {
        let mut db = open_and_init(&home, &["bank"]);
        commit_n_blocks(&mut db, 10);
        assert_eq!(db.version(), 10);
        db.close().unwrap();
    }

    // Create a fake -tmp directory to simulate interrupted snapshot
    let committer_path = dir.path().join("data").join("committer.db");
    let tmp_snapshot_dir = committer_path.join("snapshot-00000000000000000010-tmp");
    fs::create_dir_all(&tmp_snapshot_dir).unwrap();
    // Put a dummy file in it to make it non-empty
    fs::write(tmp_snapshot_dir.join("dummy"), b"interrupted").unwrap();
    assert!(tmp_snapshot_dir.exists(), "tmp dir should exist before reopen");

    // Reopen: remove_tmp_dirs should clean up the leftover -tmp directory
    {
        let mut db = SeiDb::open(&home, sc_config(), None).unwrap();
        db.initialize(&["bank".into()]);
        db.load_version(0).unwrap();

        assert_eq!(db.version(), 10, "version should still be 10 after cleaning up tmp dirs");
        assert!(
            !tmp_snapshot_dir.exists(),
            "tmp snapshot directory should have been cleaned up on reopen"
        );
        db.close().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Test 3: Clean close and reopen — WAL replay keeps data intact
// ---------------------------------------------------------------------------

/// Commits 20 blocks, closes (simulating clean shutdown), reopens, and verifies
/// all data survived the round-trip. Then commits 5 more blocks to verify
/// continued operation.
#[test]
fn test_crash_reopen_data_intact() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    // Phase 1: commit 20 blocks with known data
    {
        let mut db = open_and_init(&home, &["bank"]);
        commit_n_blocks(&mut db, 20);
        assert_eq!(db.version(), 20);
        db.close().unwrap();
    }

    // Phase 2: reopen at latest (version 0 = latest) and verify
    {
        let mut db = SeiDb::open(&home, sc_config(), None).unwrap();
        db.initialize(&["bank".into()]);
        db.load_version(0).unwrap();
        assert_eq!(db.version(), 20, "after reopen, version should be 20 but got {}", db.version());

        // Verify we can continue committing from where we left off
        for i in 21..=25i64 {
            let val = format!("value_{i}");
            let cs = vec![NamedChangeSet {
                name: "bank".into(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: format!("key_{i}").into_bytes(),
                        value: val.into_bytes(),
                    }],
                }),
            }];
            db.sc_mut().apply_change_sets(&cs).unwrap();
            let version = db.sc_mut().commit().unwrap();
            assert_eq!(version, i);
        }

        assert_eq!(db.version(), 25);
        db.close().unwrap();
    }
}
