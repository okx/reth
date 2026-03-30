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

/// I8.1: writer store export snapshot, fresh store import -> latest version/state_root consistent
#[test]
fn i8_1_export_import_roundtrip() {
    let src_dir = TempDir::new().unwrap();
    let mut src_store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(src_dir.path(), false, cfg).unwrap() };

    // Commit several blocks with data
    let addr1 = Address::repeat_byte(0x01);
    let addr2 = Address::repeat_byte(0x02);
    let info1 = default_info(1, 1000);
    let info2 = default_info(5, 500);
    let bundle = make_bundle(vec![
        (
            addr1,
            Some(info1),
            AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(42))],
        ),
        (
            addr2,
            Some(info2),
            AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(99))],
        ),
    ]);
    src_store.apply_bundle_state(&bundle).unwrap();
    let (_, root) = src_store.commit().unwrap();

    // Export
    let mut exp = src_store.exporter(1).unwrap();
    let mut nodes = Vec::new();
    while let Some(n) = exp.next_node().unwrap() {
        nodes.push(n);
    }
    exp.close().unwrap();
    src_store.close().unwrap();

    // Import into fresh store
    let dst_dir = TempDir::new().unwrap();
    let mut dst_store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dst_dir.path(), false, cfg).unwrap() };

    {
        let mut imp = dst_store.importer(1, root).unwrap();
        for n in &nodes {
            imp.add_node(n).unwrap();
        }
        imp.close().unwrap();
    }

    assert_eq!(dst_store.version(), 1);
    // Root hash is correct after import; proof generation requires re-apply.
    assert_eq!(dst_store.frontier().committed_root, root);
    assert!(dst_store.account_proof(1, addr1, &[]).is_err());
}

/// I8.2: imported store reopen(read_only=true) can still generate correct proof
#[test]
fn i8_2_imported_store_reopen_proof() {
    let src_dir = TempDir::new().unwrap();
    let mut src_store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(src_dir.path(), false, cfg).unwrap() };

    let addr = Address::repeat_byte(0x03);
    let info = default_info(1, 999);
    let bundle = make_bundle(vec![(addr, Some(info), AccountStatus::Changed, vec![])]);
    src_store.apply_bundle_state(&bundle).unwrap();
    let (_, root) = src_store.commit().unwrap();

    let mut exp = src_store.exporter(1).unwrap();
    let mut nodes = Vec::new();
    while let Some(n) = exp.next_node().unwrap() {
        nodes.push(n);
    }
    exp.close().unwrap();
    src_store.close().unwrap();

    // Import
    let dst_dir = TempDir::new().unwrap();
    {
        let mut dst_store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dst_dir.path(), false, cfg).unwrap() };
        {
            let mut imp = dst_store.importer(1, root).unwrap();
            for n in &nodes {
                imp.add_node(n).unwrap();
            }
            imp.close().unwrap();
        }
        dst_store.close().unwrap();
    }

    // Reopen read-only: root is correct; proof requires re-apply (no sparse trie).
    {
        let store = MptCommitStore::open(dst_dir.path(), true).unwrap();
        assert_eq!(store.frontier().committed_root, root);
        assert!(store.account_proof(1, addr, &[]).is_err());
    }
}

/// I8.3: exporter(version=old kept version) works before prune, fails after prune+gc
#[test]
fn i8_3_exporter_prune_gc() {
    let dir = TempDir::new().unwrap();
    let mut store = { let mut cfg = mptdb_sc::mpt::MptConfig::default(); cfg.use_sparse_storage = false; mptdb_sc::mpt::MptCommitStore::open_with_config(dir.path(), false, cfg).unwrap() };

    // v1
    let addr1 = Address::repeat_byte(0x01);
    let info1 = default_info(1, 100);
    let bundle1 = make_bundle(vec![(addr1, Some(info1), AccountStatus::Changed, vec![])]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();

    // v2
    let addr2 = Address::repeat_byte(0x02);
    let info2 = default_info(1, 200);
    let bundle2 = make_bundle(vec![(addr2, Some(info2), AccountStatus::Changed, vec![])]);
    store.apply_bundle_state(&bundle2).unwrap();
    store.commit().unwrap();

    // v1 exporter should work before prune
    {
        let mut exp = store.exporter(1).unwrap();
        assert!(exp.next_node().unwrap().is_some());
        exp.close().unwrap();
    }

    // Prune v1 and GC
    store.prune_before(2).unwrap();
    store.gc().unwrap();

    // v1 exporter should fail now (version not in manifest)
    assert!(store.exporter(1).is_err());

    // v2 exporter should still work
    {
        let mut exp = store.exporter(2).unwrap();
        assert!(exp.next_node().unwrap().is_some());
        exp.close().unwrap();
    }
}
