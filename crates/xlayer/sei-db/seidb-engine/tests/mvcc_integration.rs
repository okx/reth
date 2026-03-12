//! Integration tests exercising `MvccDatabase` through the `StateStore` trait.

use seidb_common::config::StateStoreConfig;
use seidb_engine::mvcc::db::MvccDatabase;
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_traits::ss::StateStore;
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

fn test_config_no_keep(dir: &std::path::Path) -> StateStoreConfig {
    StateStoreConfig {
        db_directory: dir.to_string_lossy().to_string(),
        keep_last_version: false,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_close_idempotent() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let mut db = MvccDatabase::open_db(&cfg).unwrap();
    db.close().unwrap();
    db.close().unwrap(); // second close must not panic
}

#[test]
fn test_latest_version() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());

    {
        let db = MvccDatabase::open_db(&cfg).unwrap();
        assert_eq!(StateStore::get_latest_version(&db), 0);
        StateStore::set_latest_version(&db, 42).unwrap();
        assert_eq!(StateStore::get_latest_version(&db), 42);
    }

    // Reopen and verify persistence.
    {
        let db = MvccDatabase::open_db(&cfg).unwrap();
        assert_eq!(StateStore::get_latest_version(&db), 42);
    }
}

#[test]
fn test_versioned_keys() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // Write 10 versions of the same key.
    for v in 1..=10 {
        let val = format!("val_{v}");
        let cs = make_changeset("store", vec![(b"key", Some(val.as_bytes()))]);
        StateStore::apply_changeset_sync(&db, v, &cs).unwrap();
    }

    // Each version should return the correct value.
    for v in 1..=10 {
        let expected = format!("val_{v}");
        assert_eq!(
            StateStore::get(&db, "store", v, b"key").unwrap(),
            Some(expected.into_bytes()),
            "mismatch at version {v}"
        );
    }
}

#[test]
fn test_get_versioned_key() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs1 = make_changeset("store", vec![(b"key", Some(b"first"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"key", Some(b"second"))]);
    StateStore::apply_changeset_sync(&db, 5, &cs2).unwrap();

    // Version 1 returns first, version 5 returns second, version 3 returns first (latest <= 3).
    assert_eq!(StateStore::get(&db, "store", 1, b"key").unwrap(), Some(b"first".to_vec()));
    assert_eq!(StateStore::get(&db, "store", 3, b"key").unwrap(), Some(b"first".to_vec()));
    assert_eq!(StateStore::get(&db, "store", 5, b"key").unwrap(), Some(b"second".to_vec()));
}

#[test]
fn test_changeset_with_delete() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // Set at v1, delete at v2 via changeset.
    let cs1 = make_changeset("store", vec![(b"key", Some(b"alive"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"key", None)]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    assert_eq!(StateStore::get(&db, "store", 1, b"key").unwrap(), Some(b"alive".to_vec()));
    assert_eq!(StateStore::get(&db, "store", 2, b"key").unwrap(), None);
    assert_eq!(StateStore::get(&db, "store", 3, b"key").unwrap(), None);
}

#[test]
fn test_changeset_multi_store() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs_bank = make_changeset("bank", vec![(b"addr1", Some(b"100"))]);
    let cs_staking = make_changeset("staking", vec![(b"val1", Some(b"power"))]);

    StateStore::apply_changeset_sync(&db, 1, &cs_bank).unwrap();
    StateStore::apply_changeset_sync(&db, 1, &cs_staking).unwrap();

    assert_eq!(StateStore::get(&db, "bank", 1, b"addr1").unwrap(), Some(b"100".to_vec()));
    assert_eq!(StateStore::get(&db, "staking", 1, b"val1").unwrap(), Some(b"power".to_vec()));

    // Cross-store isolation: bank key not in staking and vice versa.
    assert_eq!(StateStore::get(&db, "bank", 1, b"val1").unwrap(), None);
    assert_eq!(StateStore::get(&db, "staking", 1, b"addr1").unwrap(), None);
}

#[test]
fn test_has() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs = make_changeset("store", vec![(b"key", Some(b"val"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs).unwrap();

    assert!(StateStore::has(&db, "store", 1, b"key").unwrap());
    assert!(!StateStore::has(&db, "store", 1, b"missing").unwrap());
}

#[test]
fn test_import() {
    use seidb_traits::types::SnapshotNode;

    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let (tx, rx) = crossbeam_channel::bounded(128);

    let producer = std::thread::spawn(move || {
        for i in 0..100u32 {
            tx.send(SnapshotNode {
                store_key: "store".to_string(),
                key: format!("key_{i:04}").into_bytes(),
                value: format!("val_{i}").into_bytes(),
            })
            .unwrap();
        }
    });

    StateStore::import(&db, 10, rx).unwrap();
    producer.join().unwrap();

    assert_eq!(StateStore::get_latest_version(&db), 10);

    for i in 0..100u32 {
        let key = format!("key_{i:04}");
        let expected = format!("val_{i}");
        assert_eq!(
            StateStore::get(&db, "store", 10, key.as_bytes()).unwrap(),
            Some(expected.into_bytes()),
            "missing key {key}"
        );
    }
}

#[test]
fn test_tombstone_handling() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // Write at v1, delete at v2, re-write at v3.
    let cs1 = make_changeset("store", vec![(b"key", Some(b"first"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"key", None)]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    let cs3 = make_changeset("store", vec![(b"key", Some(b"revived"))]);
    StateStore::apply_changeset_sync(&db, 3, &cs3).unwrap();

    assert_eq!(StateStore::get(&db, "store", 1, b"key").unwrap(), Some(b"first".to_vec()));
    assert_eq!(StateStore::get(&db, "store", 2, b"key").unwrap(), None);
    assert_eq!(StateStore::get(&db, "store", 3, b"key").unwrap(), Some(b"revived".to_vec()));
}

#[test]
fn test_iterator_forward() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs = make_changeset(
        "store",
        vec![(b"a", Some(b"val_a")), (b"b", Some(b"val_b")), (b"c", Some(b"val_c"))],
    );
    StateStore::apply_changeset_sync(&db, 1, &cs).unwrap();

    let mut iter = StateStore::iterator(&db, "store", 1, b"", b"").unwrap();

    assert!(iter.valid());
    assert_eq!(iter.key(), b"a");
    assert_eq!(iter.value(), b"val_a");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"b");
    assert_eq!(iter.value(), b"val_b");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"c");
    assert_eq!(iter.value(), b"val_c");

    iter.next();
    assert!(!iter.valid());
}

#[test]
fn test_iterator_reverse() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs = make_changeset(
        "store",
        vec![(b"a", Some(b"val_a")), (b"b", Some(b"val_b")), (b"c", Some(b"val_c"))],
    );
    StateStore::apply_changeset_sync(&db, 1, &cs).unwrap();

    let mut iter = StateStore::reverse_iterator(&db, "store", 1, b"", b"").unwrap();

    assert!(iter.valid());
    assert_eq!(iter.key(), b"c");
    assert_eq!(iter.value(), b"val_c");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"b");
    assert_eq!(iter.value(), b"val_b");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"a");
    assert_eq!(iter.value(), b"val_a");

    iter.next();
    assert!(!iter.valid());
}

#[test]
fn test_iterator_range() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs = make_changeset(
        "store",
        vec![(b"a", Some(b"va")), (b"b", Some(b"vb")), (b"c", Some(b"vc")), (b"d", Some(b"vd"))],
    );
    StateStore::apply_changeset_sync(&db, 1, &cs).unwrap();

    // Range [b, d) should yield b, c.
    let mut iter = StateStore::iterator(&db, "store", 1, b"b", b"d").unwrap();

    assert!(iter.valid());
    assert_eq!(iter.key(), b"b");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"c");

    iter.next();
    assert!(!iter.valid());
}

#[test]
fn test_iterator_version_aware() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs1 = make_changeset("store", vec![(b"a", Some(b"a_v1")), (b"b", Some(b"b_v1"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"a", Some(b"a_v2"))]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    // At version 1, should see a_v1.
    let iter = StateStore::iterator(&db, "store", 1, b"", b"").unwrap();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"a");
    assert_eq!(iter.value(), b"a_v1");

    // At version 2, should see a_v2.
    let iter = StateStore::iterator(&db, "store", 2, b"", b"").unwrap();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"a");
    assert_eq!(iter.value(), b"a_v2");
}

#[test]
fn test_iterator_tombstone_skip() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs1 = make_changeset(
        "store",
        vec![(b"a", Some(b"va")), (b"b", Some(b"vb")), (b"c", Some(b"vc"))],
    );
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    // Delete "b" at v2.
    let cs2 = make_changeset("store", vec![(b"b", None)]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    // Iterator at v2 should skip "b".
    let mut iter = StateStore::iterator(&db, "store", 2, b"", b"").unwrap();

    assert!(iter.valid());
    assert_eq!(iter.key(), b"a");

    iter.next();
    assert!(iter.valid());
    assert_eq!(iter.key(), b"c");

    iter.next();
    assert!(!iter.valid());
}

#[test]
fn test_iterator_empty() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // No data written - iterator should be immediately invalid.
    let iter = StateStore::iterator(&db, "store", 1, b"", b"").unwrap();
    assert!(!iter.valid());
}

#[test]
fn test_prune_basic() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // Write three versions.
    for v in 1..=3 {
        let val = format!("v{v}");
        let cs = make_changeset("store", vec![(b"key", Some(val.as_bytes()))]);
        StateStore::apply_changeset_sync(&db, v, &cs).unwrap();
    }

    StateStore::prune(&db, 2).unwrap();

    // Earliest version should advance to 3.
    assert_eq!(StateStore::get_earliest_version(&db), 3);

    // Version 3 should still be readable.
    assert_eq!(StateStore::get(&db, "store", 3, b"key").unwrap(), Some(b"v3".to_vec()));
}

#[test]
fn test_prune_keep_last_version() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path()); // keep_last_version=true
    let db = MvccDatabase::open_db(&cfg).unwrap();

    // Single version at prune height.
    let cs = make_changeset("store", vec![(b"solo", Some(b"only"))]);
    StateStore::apply_changeset_sync(&db, 2, &cs).unwrap();

    StateStore::prune(&db, 2).unwrap();

    // With keep_last_version=true, the last version should survive.
    // earliest_version is now 3, so query at v3 which finds v2 entry.
    assert_eq!(StateStore::get(&db, "store", 3, b"solo").unwrap(), Some(b"only".to_vec()));
}

#[test]
fn test_prune_tombstone() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs1 = make_changeset("store", vec![(b"key", Some(b"alive"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"key", None)]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    StateStore::prune(&db, 2).unwrap();

    // Both the value and tombstone should be physically removed.
    // earliest_version is now 3. Any query should return None.
    assert_eq!(StateStore::get(&db, "store", 3, b"key").unwrap(), None);
}

#[test]
fn test_raw_iterate() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs1 = make_changeset("store", vec![(b"a", Some(b"a_v1"))]);
    StateStore::apply_changeset_sync(&db, 1, &cs1).unwrap();

    let cs2 = make_changeset("store", vec![(b"a", Some(b"a_v2")), (b"b", Some(b"b_v1"))]);
    StateStore::apply_changeset_sync(&db, 2, &cs2).unwrap();

    let mut entries: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
    let stopped = StateStore::raw_iterate(&db, "store", &mut |key, val, ver| {
        entries.push((key.to_vec(), val.to_vec(), ver));
        false
    })
    .unwrap();

    assert!(!stopped);
    // raw_iterate shows all versions: (a,v1), (a,v2), (b,v2)
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].0, b"a");
    assert_eq!(entries[0].2, 1);
    assert_eq!(entries[1].0, b"a");
    assert_eq!(entries[1].2, 2);
    assert_eq!(entries[2].0, b"b");
    assert_eq!(entries[2].2, 2);
}

#[test]
fn test_persistence_after_close() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());

    // Write data, close, reopen, verify.
    {
        let mut db = MvccDatabase::open_db(&cfg).unwrap();
        let cs = make_changeset("store", vec![(b"key1", Some(b"val1")), (b"key2", Some(b"val2"))]);
        StateStore::apply_changeset_sync(&db, 1, &cs).unwrap();
        StateStore::set_latest_version(&db, 1).unwrap();
        db.close().unwrap();
    }

    {
        let db = MvccDatabase::open_db(&cfg).unwrap();
        assert_eq!(StateStore::get_latest_version(&db), 1);
        assert_eq!(StateStore::get(&db, "store", 1, b"key1").unwrap(), Some(b"val1".to_vec()));
        assert_eq!(StateStore::get(&db, "store", 1, b"key2").unwrap(), Some(b"val2".to_vec()));
    }
}

#[test]
fn test_earliest_version_conditional() {
    let dir = tempdir().unwrap();
    let cfg = test_config(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    StateStore::set_earliest_version(&db, 50, false).unwrap();
    assert_eq!(StateStore::get_earliest_version(&db), 50);

    // Lower value without ignore_version should be a no-op.
    StateStore::set_earliest_version(&db, 30, false).unwrap();
    assert_eq!(StateStore::get_earliest_version(&db), 50);

    // With ignore_version=true, unconditional update.
    StateStore::set_earliest_version(&db, 10, true).unwrap();
    assert_eq!(StateStore::get_earliest_version(&db), 10);
}

#[test]
fn test_prune_no_keep_last_version() {
    let dir = tempdir().unwrap();
    let cfg = test_config_no_keep(dir.path());
    let db = MvccDatabase::open_db(&cfg).unwrap();

    let cs = make_changeset("store", vec![(b"key", Some(b"val"))]);
    StateStore::apply_changeset_sync(&db, 2, &cs).unwrap();

    StateStore::prune(&db, 2).unwrap();

    // With keep_last_version=false, even the last version at prune height
    // should be deleted.
    assert_eq!(StateStore::get(&db, "store", 2, b"key").unwrap(), None);
}
