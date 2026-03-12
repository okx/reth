//! P0 acceptance test for MVCC database (A-07).
//!
//! Ported from Go TestParallelWrites and TestParallelReadsWrites in
//! sei-db/db_engine/test/storage_test_suite.go.

use seidb_common::config::StateStoreConfig;
use seidb_engine::mvcc::db::MvccDatabase;
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use std::sync::Arc;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(dir: &std::path::Path) -> StateStoreConfig {
    StateStoreConfig {
        db_directory: dir.to_string_lossy().to_string(),
        keep_last_version: true,
        ..Default::default()
    }
}

fn make_changeset(store: &str, pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
    vec![NamedChangeSet { name: store.to_string(), changeset: Some(ChangeSet { pairs }) }]
}

fn kv_set(key: &[u8], value: &[u8]) -> KvPair {
    KvPair { delete: false, key: key.to_vec(), value: value.to_vec() }
}

// ===========================================================================
// A-07: MVCC parallel reads and writes
// ===========================================================================
// Ported from Go TestParallelWrites + TestParallelReadsWrites.
//
// Spawn 4 writer threads, each writing different store keys concurrently via
// apply_changeset_sync. Then spawn 4 reader threads + 1 writer thread
// concurrently. No panics, no data races, final state consistent.

#[test]
fn a07_mvcc_parallel_writes() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = Arc::new(MvccDatabase::open_db(&cfg).unwrap());

    let num_writers = 4;
    let writes_per_thread = 50;

    // Phase 1: 4 writer threads writing different store keys concurrently
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for writer_id in 0..num_writers {
            let db_ref = Arc::clone(&db);
            handles.push(s.spawn(move || {
                let store_key = format!("store{}", writer_id);
                for i in 0..writes_per_thread {
                    let version = (writer_id * writes_per_thread + i + 1) as i64;
                    let key = format!("key_{writer_id}_{i}");
                    let value = format!("value_{writer_id}_{i}");
                    let cs =
                        make_changeset(&store_key, vec![kv_set(key.as_bytes(), value.as_bytes())]);
                    db_ref.apply_changeset_sync(version, &cs).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    // Verify all writes landed correctly
    for writer_id in 0..num_writers {
        let store_key = format!("store{}", writer_id);
        for i in 0..writes_per_thread {
            let version = (writer_id * writes_per_thread + i + 1) as i64;
            let key = format!("key_{writer_id}_{i}");
            let expected = format!("value_{writer_id}_{i}");
            let val = db.get(&store_key, version, key.as_bytes()).unwrap();
            assert_eq!(
                val,
                Some(expected.into_bytes()),
                "writer {writer_id} key {i} should be readable at version {version}"
            );
        }
    }
}

#[test]
fn a07_mvcc_parallel_reads_and_writes() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = Arc::new(MvccDatabase::open_db(&cfg).unwrap());

    // Seed initial data at version 1
    let seed_pairs: Vec<KvPair> = (0..100)
        .map(|i| {
            let key = format!("seed_key_{i}");
            let value = format!("seed_value_{i}");
            kv_set(key.as_bytes(), value.as_bytes())
        })
        .collect();
    db.apply_changeset_sync(1, &make_changeset("test", seed_pairs)).unwrap();
    db.set_latest_version(1).unwrap();

    // Phase 2: concurrent readers + 1 writer
    std::thread::scope(|s| {
        let mut handles = Vec::new();

        // 4 reader threads reading existing data
        for reader_id in 0..4u32 {
            let db_ref = Arc::clone(&db);
            handles.push(s.spawn(move || {
                for i in 0..100 {
                    let key = format!("seed_key_{i}");
                    let val = db_ref.get("test", 1, key.as_bytes()).unwrap();
                    assert!(
                        val.is_some(),
                        "reader {reader_id}: seed_key_{i} should exist at version 1"
                    );
                    let expected = format!("seed_value_{i}");
                    assert_eq!(val.unwrap(), expected.into_bytes());
                }
            }));
        }

        // 1 writer thread writing new data at version 2
        {
            let db_ref = Arc::clone(&db);
            handles.push(s.spawn(move || {
                for i in 0..50 {
                    let key = format!("new_key_{i}");
                    let value = format!("new_value_{i}");
                    let cs = make_changeset("test", vec![kv_set(key.as_bytes(), value.as_bytes())]);
                    db_ref.apply_changeset_sync(2, &cs).unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    });

    // Verify the writer's data is consistent
    for i in 0..50 {
        let key = format!("new_key_{i}");
        let expected = format!("new_value_{i}");
        let val = db.get("test", 2, key.as_bytes()).unwrap();
        assert_eq!(val, Some(expected.into_bytes()), "new_key_{i} should exist at version 2");
    }

    // Verify seed data is still accessible at version 1
    for i in 0..100 {
        let key = format!("seed_key_{i}");
        let expected = format!("seed_value_{i}");
        let val = db.get("test", 1, key.as_bytes()).unwrap();
        assert_eq!(
            val,
            Some(expected.into_bytes()),
            "seed_key_{i} should still exist at version 1"
        );
    }
}
