//! P0 acceptance tests for FlatKV CommitStore (A-01 through A-06).
//!
//! Ported from the Go reference tests in sei-db/state_db/sc/flatkv/*.

use seidb_common::{
    config::FlatKvConfig,
    evm_keys::{CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_sc::flatkv::store::CommitStore;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
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

fn nonce_key(addr_seed: u8) -> Vec<u8> {
    let mut k = vec![NONCE_KEY_PREFIX];
    let mut addr = [0u8; 20];
    addr[0] = addr_seed;
    k.extend_from_slice(&addr);
    k
}

fn codehash_key(addr_seed: u8) -> Vec<u8> {
    let mut k = vec![CODE_HASH_KEY_PREFIX];
    let mut addr = [0u8; 20];
    addr[0] = addr_seed;
    k.extend_from_slice(&addr);
    k
}

fn code_key(addr_seed: u8) -> Vec<u8> {
    let mut k = vec![CODE_KEY_PREFIX];
    let mut addr = [0u8; 20];
    addr[0] = addr_seed;
    k.extend_from_slice(&addr);
    k
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

fn legacy_key(seed: u8) -> Vec<u8> {
    let mut addr = [0u8; 20];
    addr[0] = seed;
    let mut k = vec![0x09]; // CodeSize prefix -> routes to legacy
    k.extend_from_slice(&addr);
    k
}

fn encode_nonce(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

fn decode_nonce(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().unwrap())
}

fn commit_and_check(store: &mut CommitStore) -> i64 {
    store.commit().unwrap()
}

// ===========================================================================
// A-01: Multiple ApplyChangeSets — account field preservation
// ===========================================================================
// Ported from Go TestMultipleApplyAccountFieldsPreservesOther.
//
// Apply changeset 1 with nonce update for address A. Apply changeset 2 with
// codehash update for same address A. After commit, both nonce AND codehash
// should be preserved (nonce not wiped by codehash update).

#[test]
fn a01_multiple_apply_account_fields_preserves_other() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    let addr_seed = 0xBB;
    let nk = nonce_key(addr_seed);
    let chk = codehash_key(addr_seed);
    let code_hash: [u8; 32] = {
        let mut h = [0u8; 32];
        h[0] = 0xDE;
        h[1] = 0xAD;
        h[2] = 0xBE;
        h[3] = 0xEF;
        h[31] = 0x01;
        h
    };

    // Version 1: set nonce = 42
    let cs1 = make_evm_cs(vec![kv(nk.clone(), encode_nonce(42))]);
    store.apply_change_sets(&cs1).unwrap();
    commit_and_check(&mut store);

    // Version 2: set codehash (different field, same address)
    let cs2 = make_evm_cs(vec![kv(chk.clone(), code_hash.to_vec())]);
    store.apply_change_sets(&cs2).unwrap();
    commit_and_check(&mut store);

    // Verify nonce is PRESERVED after codehash update
    let (nonce_val, found) = store.get(&nk);
    assert!(found, "nonce should be found after codehash update");
    assert_eq!(
        decode_nonce(&nonce_val.unwrap()),
        42,
        "nonce should be preserved after codehash update"
    );

    // Verify codehash is set
    let (ch_val, found) = store.get(&chk);
    assert!(found, "codehash should be found");
    assert_eq!(ch_val.unwrap(), code_hash.to_vec());

    store.close().unwrap();
}

// ===========================================================================
// A-02: Same storage key multiple times in one changeset
// ===========================================================================
// Ported from Go TestLtHashSameStorageKeyMultipleTimesInOneChangeset.
//
// One changeset contains the same storage key written twice (with different
// values). LtHash should reflect only the final value. After commit the
// stored value must be the last-write-wins value and the LtHash must be
// consistent with a full recomputation.

#[test]
fn a02_same_storage_key_multiple_times_in_one_changeset() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    let sk = storage_key(0x01, 0x01);

    // Write same key twice in one changeset — last write wins
    let cs = make_evm_cs(vec![kv(sk.clone(), vec![0x11]), kv(sk.clone(), vec![0x22])]);
    store.apply_change_sets(&cs).unwrap();
    commit_and_check(&mut store);

    // Value should be 0x22 (last write wins)
    let (val, found) = store.get(&sk);
    assert!(found, "storage key should exist");
    assert_eq!(val.unwrap(), vec![0x22], "last write should win");

    // LtHash consistency: reopen and verify the hash matches
    let hash_before_close = store.root_hash();
    store.close().unwrap();

    let mut store2 = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
    store2.load_version(0).unwrap();
    assert_eq!(store2.version(), 1);
    assert_eq!(store2.root_hash(), hash_before_close, "LtHash must match after reopen");
    store2.close().unwrap();
}

// ===========================================================================
// A-03: Non-storage key routing (all EVM key types)
// ===========================================================================
// Ported from Go TestStoreNonStorageKeys + TestStoreWriteAllDBs.
//
// Apply changesets with all EVM key types: nonce (0x0a), codehash (0x08),
// code (0x07), storage (0x03), and legacy (0x09). After commit, verify
// each key is readable via the Store.Get method.

#[test]
fn a03_non_storage_key_routing() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    let addr_seed = 0x99;
    let slot_seed = 0x88;

    // Build keys for all types
    let nk = nonce_key(addr_seed);
    let chk = codehash_key(addr_seed);
    let ck = code_key(addr_seed);
    let sk = storage_key(addr_seed, slot_seed);
    let lk = legacy_key(addr_seed);

    let code_hash: [u8; 32] = {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = ((i + 0x11) & 0xFF) as u8;
        }
        h
    };

    // Apply all key types in one changeset
    let cs = make_evm_cs(vec![
        kv(nk.clone(), encode_nonce(17)),
        kv(chk.clone(), code_hash.to_vec()),
        kv(ck.clone(), vec![0x60, 0x60, 0x60]),
        kv(sk.clone(), vec![0x11, 0x22]),
        kv(lk.clone(), vec![0x00, 0x10]),
    ]);
    store.apply_change_sets(&cs).unwrap();
    commit_and_check(&mut store);

    // Verify nonce
    let (val, found) = store.get(&nk);
    assert!(found, "nonce should be found");
    assert_eq!(decode_nonce(&val.unwrap()), 17);

    // Verify codehash
    let (val, found) = store.get(&chk);
    assert!(found, "codehash should be found");
    assert_eq!(val.unwrap(), code_hash.to_vec());

    // Verify code
    let (val, found) = store.get(&ck);
    assert!(found, "code should be found");
    assert_eq!(val.unwrap(), vec![0x60, 0x60, 0x60]);

    // Verify storage
    let (val, found) = store.get(&sk);
    assert!(found, "storage should be found");
    assert_eq!(val.unwrap(), vec![0x11, 0x22]);

    // Verify legacy
    let (val, found) = store.get(&lk);
    assert!(found, "legacy should be found");
    assert_eq!(val.unwrap(), vec![0x00, 0x10]);

    store.close().unwrap();
}

// ===========================================================================
// A-04: Legacy key included in LtHash
// ===========================================================================
// Ported from Go TestStoreLegacyKeyIncludedInLtHash.
//
// Write a legacy key (e.g. CodeSize prefix 0x09). Verify that the LtHash
// changes (legacy keys participate in LtHash computation).

#[test]
fn a04_legacy_key_included_in_lthash() {
    let dir = tempdir().unwrap();
    let mut store = new_store(dir.path().to_str().unwrap());

    // Initial hash
    let hash1 = store.root_hash();

    // Write a legacy key
    let lk = legacy_key(0xDD);
    let cs = make_evm_cs(vec![kv(lk.clone(), vec![0x00, 0x20])]);
    store.apply_change_sets(&cs).unwrap();

    // LtHash should change after applying legacy key changeset
    let hash2 = store.root_hash();
    assert_ne!(hash1, hash2, "LtHash should change when legacy key is written");

    commit_and_check(&mut store);

    // After commit, hash should be stable
    let hash3 = store.root_hash();
    assert_eq!(hash2, hash3, "LtHash should be stable after commit");

    store.close().unwrap();
}

// ===========================================================================
// A-05: WAL clear (clearChangelog) — tested via close/reopen cycle
// ===========================================================================
// Ported from Go TestClearChangelog.
//
// Since clear_changelog is private, we test the observable behavior:
// commit 5 versions, write a snapshot, close, reopen. Data is still in DBs,
// and we can commit more versions without issue.
// The key invariant: data survives, version is correct, and new commits work.

#[test]
fn a05_wal_clear_via_snapshot_reopen() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    // Phase 1: commit 5 versions
    {
        let mut store = new_store(db_dir);
        for i in 1u8..=5 {
            let sk = storage_key(i, i);
            let cs = make_evm_cs(vec![kv(sk, vec![i])]);
            store.apply_change_sets(&cs).unwrap();
            commit_and_check(&mut store);
        }
        assert_eq!(store.version(), 5);

        // Write snapshot — this makes WAL truncation possible
        store.write_snapshot().unwrap();
        store.close().unwrap();
    }

    // Phase 2: reopen — catchup should work, data should be intact
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        store.load_version(0).unwrap();
        assert_eq!(store.version(), 5, "should recover to version 5");

        // Verify data is still readable
        for i in 1u8..=5 {
            let sk = storage_key(i, i);
            let (val, found) = store.get(&sk);
            assert!(found, "storage key {i} should still be found after reopen");
            assert_eq!(val.unwrap(), vec![i]);
        }

        // Commit 1 more version to verify everything works post-recovery
        let sk = storage_key(6, 6);
        let cs = make_evm_cs(vec![kv(sk.clone(), vec![6])]);
        store.apply_change_sets(&cs).unwrap();
        let v = commit_and_check(&mut store);
        assert_eq!(v, 6);

        let (val, found) = store.get(&sk);
        assert!(found, "new key should be found after post-recovery commit");
        assert_eq!(val.unwrap(), vec![6]);

        store.close().unwrap();
    }
}

// ===========================================================================
// A-06: Version loading validation
// ===========================================================================
// Ported from Go TestLoadVersionTargetBeyondWALFails +
// TestCatchupFromSpecificVersion.
//
// Test version loading edge cases:
// - Load version beyond WAL -> error
// - Load version 0 (latest) -> OK
// - Reopen after snapshot -> correct version via WAL catchup

#[test]
fn a06_version_loading_validation() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().to_str().unwrap();

    // Phase 1: create store, commit versions 1-5, snapshot, close
    {
        let mut store = new_store(db_dir);
        for i in 1u8..=5 {
            let sk = storage_key(i, i);
            let cs = make_evm_cs(vec![kv(sk, vec![i])]);
            store.apply_change_sets(&cs).unwrap();
            commit_and_check(&mut store);
        }
        store.write_snapshot().unwrap();

        // Commit 2 more versions beyond snapshot (WAL only)
        for i in 6u8..=7 {
            let sk = storage_key(i, i);
            let cs = make_evm_cs(vec![kv(sk, vec![i])]);
            store.apply_change_sets(&cs).unwrap();
            commit_and_check(&mut store);
        }
        assert_eq!(store.version(), 7);
        store.close().unwrap();
    }

    // Phase 2: load version beyond WAL should fail
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        let result = store.load_version(100);
        assert!(result.is_err(), "loading version beyond WAL should fail");
    }

    // Phase 3: load version 0 (latest) should succeed and catch up
    {
        let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
        store.load_version(0).unwrap();
        assert_eq!(store.version(), 7, "should catch up to latest version 7");

        // Verify data from all versions
        for i in 1u8..=7 {
            let sk = storage_key(i, i);
            let (val, found) = store.get(&sk);
            assert!(found, "key for version {i} should be found");
            assert_eq!(val.unwrap(), vec![i]);
        }

        store.close().unwrap();
    }

    // Phase 4: reopen and catchup preserves LtHash
    {
        let mut store1 = CommitStore::new(db_dir, FlatKvConfig::default());
        store1.load_version(0).unwrap();
        let hash = store1.root_hash();
        assert_eq!(hash.len(), 32, "root hash should be 32 bytes");
        // Hash should be non-trivial (we have data)
        assert_ne!(
            hash,
            vec![0u8; 32].as_slice()[..hash.len()].to_vec(),
            "root hash should not be all zeros with committed data"
        );
        store1.close().unwrap();
    }
}
