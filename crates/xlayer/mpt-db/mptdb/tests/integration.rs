//! Top-level integration tests for MptDb with MPT SC backend.
//!
//! Tests I4.1-I4.6: lifecycle, multi-block commit, reopen, rollback, SS interop.

use alloy_primitives::U256;
use mptdb::{
    db::MptDb,
    MptCommitter, // trait import for commit/rollback/load_version/close
};
use mptdb_common::config::{StateCommitConfig, StateStoreConfig};
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
use tempfile::tempdir;

fn make_bundle(
    accounts: Vec<(
        alloy_primitives::Address,
        Option<AccountInfo>,
        revm_database::AccountStatus,
        Vec<(U256, U256, U256)>,
    )>,
) -> BundleState {
    let mut state: alloy_primitives::map::AddressMap<BundleAccount> =
        alloy_primitives::map::AddressMap::default();
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

fn default_ss_config(dir: &std::path::Path) -> StateStoreConfig {
    StateStoreConfig {
        enable: true,
        db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
        keep_last_version: true,
        ..Default::default()
    }
}

/// I4.1: MptDb open / build / load / close lifecycle
#[test]
fn i4_1_lifecycle() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();
    assert_eq!(db.version(), 0);
    db.load_version(0).unwrap();
    assert_eq!(db.version(), 0);
    db.close().unwrap();
}

/// I4.2: multiple blocks of BundleState commit
#[test]
fn i4_2_multi_block_commit() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();

    // Block 1: create account
    let addr = alloy_primitives::Address::repeat_byte(0xAA);
    let bundle1 = make_bundle(vec![(
        addr,
        Some(default_info(1, 100)),
        revm_database::AccountStatus::Changed,
        vec![(U256::from(1), U256::ZERO, U256::from(42))],
    )]);
    db.sc_mut().apply_bundle_state(&bundle1).unwrap();
    let (v1, _r1) = db.sc_mut().commit().unwrap();
    assert_eq!(v1, 1);

    // Block 2: update balance
    let bundle2 = make_bundle(vec![(
        addr,
        Some(default_info(2, 200)),
        revm_database::AccountStatus::Changed,
        vec![],
    )]);
    db.sc_mut().apply_bundle_state(&bundle2).unwrap();
    let (v2, _r2) = db.sc_mut().commit().unwrap();
    assert_eq!(v2, 2);

    // Block 3: add another account
    let addr2 = alloy_primitives::Address::repeat_byte(0xBB);
    let bundle3 = make_bundle(vec![(
        addr2,
        Some(default_info(1, 500)),
        revm_database::AccountStatus::Changed,
        vec![],
    )]);
    db.sc_mut().apply_bundle_state(&bundle3).unwrap();
    let (v3, _r3) = db.sc_mut().commit().unwrap();
    assert_eq!(v3, 3);
    assert_eq!(db.version(), 3);
}

/// I4.3: reopen + latest-load
#[test]
fn i4_3_reopen_latest() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    {
        let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();
        for i in 1..=5 {
            let addr = alloy_primitives::Address::repeat_byte(i);
            let bundle = make_bundle(vec![(
                addr,
                Some(default_info(u64::from(i), u64::from(i) * 100)),
                revm_database::AccountStatus::Changed,
                vec![],
            )]);
            db.sc_mut().apply_bundle_state(&bundle).unwrap();
            db.sc_mut().commit().unwrap();
        }
        assert_eq!(db.version(), 5);
        db.close().unwrap();
    }

    let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();
    assert_eq!(db.version(), 5);
    db.load_version(0).unwrap();
    assert_eq!(db.version(), 5);
}

/// I4.4: rollback + latest-load
#[test]
fn i4_4_rollback_latest() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();

    for _ in 0..4 {
        db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
        db.sc_mut().commit().unwrap();
    }
    assert_eq!(db.version(), 4);

    db.sc_mut().rollback(2).unwrap();
    assert_eq!(db.version(), 2);

    db.load_version(0).unwrap();
    assert_eq!(db.version(), 2);

    // Can continue committing after rollback
    db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
    let (v, _) = db.sc_mut().commit().unwrap();
    assert_eq!(v, 3);
}

/// I4.5: SS enabled does not affect MPT SC path
#[test]
fn i4_5_ss_with_mpt() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let ss_config = default_ss_config(dir.path());

    let mut db = MptDb::open(&home, StateCommitConfig::default(), Some(ss_config)).unwrap();
    assert!(db.ss().is_some());

    // SC still works
    db.sc_mut().apply_bundle_state(&BundleState::default()).unwrap();
    let (ver, _) = db.sc_mut().commit().unwrap();
    assert_eq!(ver, 1);
}

/// I4.6: all commit() call sites handle (i64, B256) return value
#[test]
fn i4_6_commit_tuple_return() {
    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let mut db = MptDb::open(&home, StateCommitConfig::default(), None).unwrap();

    let addr = alloy_primitives::Address::repeat_byte(0xCC);
    let bundle = make_bundle(vec![(
        addr,
        Some(default_info(1, 1000)),
        revm_database::AccountStatus::Changed,
        vec![(U256::from(10), U256::ZERO, U256::from(999))],
    )]);

    db.sc_mut().apply_bundle_state(&bundle).unwrap();
    let (version, state_root) = db.sc_mut().commit().unwrap();
    assert_eq!(version, 1);
    // State root should not be empty since we added an account with storage
    assert_ne!(
        state_root,
        alloy_primitives::b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421")
    );
}
