//! MPT backend integration tests for SeiDb top-level.
//!
//! Tests I3.1–I3.9: verify that SeiDb's SC layer is wired directly to MptCommitStore.

use alloy_primitives::U256;
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
use seidb::{
    db::SeiDb,
    MptCommitter, // trait import for commit/rollback/load_version/close/version
};
use seidb_common::config::StateCommitConfig;
use tempfile::tempdir;

fn make_bundle(
    accounts: Vec<(
        alloy_primitives::Address,
        Option<AccountInfo>,
        revm_database::AccountStatus,
        Vec<(U256, U256, U256)>,
    )>,
) -> BundleState {
    let mut state: alloy_primitives::map::HashMap<alloy_primitives::Address, BundleAccount> =
        alloy_primitives::map::HashMap::default();
    for (address, info, status, storage) in accounts {
        let storage_map: revm_database::StorageWithOriginalValues = storage
            .into_iter()
            .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
            .collect();
        let bundle_account = BundleAccount::new(None, info, storage_map, status);
        state.insert(address, bundle_account);
    }
    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

fn default_info(nonce: u64, balance: u64) -> AccountInfo {
    AccountInfo {
        nonce,
        balance: U256::from(balance),
        code_hash: alloy_primitives::b256!(
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        ),
        account_id: None,
        code: None,
    }
}

/// I3.1: build -> version == 0
#[test]
fn i3_1_build_version_zero() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
    assert_eq!(db.version(), 0);
}

/// I3.2: load_version(0) succeeds
#[test]
fn i3_2_load_version_zero() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
    assert!(db.load_version(0).is_ok());
}

/// I3.3: load_version(1) returns Err
#[test]
fn i3_3_load_version_nonzero() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
    let result = db.load_version(1);
    assert!(result.is_err());
}

/// I3.4: apply_bundle_state + commit works, db.version() increments
#[test]
fn i3_4_apply_commit() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();

    let addr = alloy_primitives::Address::repeat_byte(0x01);
    let info = default_info(1, 1000);
    let bundle =
        make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);

    db.sc_mut().apply_bundle_state(&bundle).unwrap();
    let (ver, _root) = db.sc_mut().commit().unwrap();
    assert_eq!(ver, 1);
    assert_eq!(db.version(), 1);
}

/// I3.5: close + reopen preserves latest version
#[test]
fn i3_5_close_reopen() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    {
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
        db.sc_mut().commit().unwrap();
        db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
        db.sc_mut().commit().unwrap();
        assert_eq!(db.version(), 2);
        db.close().unwrap();
    }

    let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
    assert_eq!(db.version(), 2);
}

/// I3.6: rollback then load_version(0)
#[test]
fn i3_6_rollback_then_load() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();

    for _ in 0..3 {
        db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
        db.sc_mut().commit().unwrap();
    }
    assert_eq!(db.version(), 3);

    db.sc_mut().rollback(1).unwrap();
    assert_eq!(db.version(), 1);

    db.load_version(0).unwrap();
    assert_eq!(db.version(), 1);
}

/// I3.7: initialize is no-op, does not affect commit
#[test]
fn i3_7_initialize_noop() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();

    db.initialize(&["bank".to_string(), "staking".to_string()]);

    db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
    let (ver, _) = db.sc_mut().commit().unwrap();
    assert_eq!(ver, 1);
}

/// I3.8: top-level test uses `seidb::MptCommitter` for trait methods
#[test]
fn i3_8_trait_import() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();

    // These calls resolve through MptCommitter trait
    db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
    let (_ver, _root) = db.sc_mut().commit().unwrap();
    db.sc_mut().load_version().unwrap();
    assert_eq!(db.sc().version(), 1);
    db.sc_mut().close().unwrap();
}

/// I3.9: commit() returns (i64, B256), destructured properly
#[test]
fn i3_9_commit_returns_tuple() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();

    db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
    let (version, _state_root) = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);
}
