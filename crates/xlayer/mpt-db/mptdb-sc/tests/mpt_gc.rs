use alloy_primitives::{Address, U256};
use alloy_trie::KECCAK_EMPTY;
use mptdb_sc::mpt::{MptCommitStore, MptCommitter};
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;
use tempfile::TempDir;

fn make_bundle(
    accounts: Vec<(Address, Option<AccountInfo>, AccountStatus, Vec<(U256, U256, U256)>)>,
) -> BundleState {
    let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
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
        code_hash: KECCAK_EMPTY,
        account_id: None,
        code: None,
    }
}

/// I9.1: multi-version commit -> prune_before -> gc -> orphan nodes decrease
#[test]
fn i9_1_prune_gc_reduces_orphans() {
    let dir = TempDir::new().unwrap();
    let mut store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dir.path(), false, cfg).unwrap() };

    // v1: addr1 with lots of storage to create many nodes
    let addr1 = Address::repeat_byte(0x01);
    let info1 = default_info(1, 100);
    let slots1: Vec<(U256, U256, U256)> =
        (0..20).map(|i| (U256::from(i), U256::ZERO, U256::from(i + 1))).collect();
    let bundle1 = make_bundle(vec![(addr1, Some(info1), AccountStatus::Changed, slots1)]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();
        store.flush_persist().unwrap();

    // v2: completely different state (addr2)
    let addr2 = Address::repeat_byte(0x02);
    let info2 = default_info(1, 200);
    let slots2: Vec<(U256, U256, U256)> =
        (100..120).map(|i| (U256::from(i), U256::ZERO, U256::from(i + 1))).collect();
    let bundle2 = make_bundle(vec![(addr2, Some(info2), AccountStatus::Changed, slots2)]);
    store.apply_bundle_state(&bundle2).unwrap();
    store.commit().unwrap();
        store.flush_persist().unwrap();

    // Before prune: gc should not delete anything (all versions retained)
    let stats_before = store.gc().unwrap();
    assert_eq!(stats_before.deleted_nodes, 0);

    // Prune v1
    store.prune_before(2).unwrap();

    // After prune: gc should delete v1's orphan nodes
    let stats_after = store.gc().unwrap();
    assert!(
        stats_after.deleted_nodes > 0,
        "expected some orphan nodes to be deleted, got stats: {:?}",
        stats_after
    );
}

/// I9.2: gc after prune -> latest version still works: reopen/load_version/proof
#[test]
fn i9_2_gc_latest_still_works() {
    let dir = TempDir::new().unwrap();
    let mut store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dir.path(), false, cfg).unwrap() };

    let addr = Address::repeat_byte(0x01);

    // v1
    let info1 = default_info(1, 100);
    let bundle1 = make_bundle(vec![(
        addr,
        Some(info1),
        AccountStatus::Changed,
        vec![(U256::from(1), U256::ZERO, U256::from(42))],
    )]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();
        store.flush_persist().unwrap();

    // v2
    let info2 = default_info(2, 200);
    let bundle2 = make_bundle(vec![(
        addr,
        Some(info2),
        AccountStatus::Changed,
        vec![(U256::from(1), U256::from(42), U256::from(99))],
    )]);
    store.apply_bundle_state(&bundle2).unwrap();
    let (_, root2) = store.commit().unwrap();
        store.flush_persist().unwrap();

    // Prune + GC
    store.prune_before(2).unwrap();
    store.gc().unwrap();
    store.close().unwrap();

    // Reopen and verify the latest committed root is intact after GC.
    let store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dir.path(), false, cfg).unwrap() };
    assert_eq!(store.version(), 2);
    assert_eq!(store.frontier().committed_root, root2, "committed root must survive GC");
}

/// I9.3: shared reader prevents writer/GC (lock conflict)
#[test]
fn i9_3_reader_blocks_writer() {
    let dir = TempDir::new().unwrap();

    // Open as reader
    let _reader = MptCommitStore::open(dir.path(), true).unwrap();

    // Writer should fail due to lock
    let result = MptCommitStore::open(dir.path(), false);
    assert!(result.is_err(), "writer should be blocked by reader's shared lock");
}
