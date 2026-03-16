use alloy_primitives::{Address, B256, U256};
use alloy_trie::KECCAK_EMPTY;
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
use seidb_sc::mpt::{MptCommitStore, MptCommitter};
use tempfile::TempDir;

fn make_bundle(
    accounts: Vec<(
        Address,
        Option<AccountInfo>,
        revm_database::AccountStatus,
        Vec<(U256, U256, U256)>,
    )>,
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

/// Create a storage-heavy bundle: many accounts each with many storage slots.
fn storage_heavy_bundle(num_accounts: usize, slots_per_account: usize) -> BundleState {
    let mut accounts = Vec::new();
    for i in 0..num_accounts {
        let addr = Address::from_word(B256::from(U256::from(i + 1)));
        let info = default_info(1, 1000 + i as u64);
        let storage: Vec<(U256, U256, U256)> = (0..slots_per_account)
            .map(|s| (U256::from(s), U256::ZERO, U256::from(s + 1)))
            .collect();
        accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, storage));
    }
    make_bundle(accounts)
}

/// Create an account-heavy bundle: many accounts with no or minimal storage.
fn account_heavy_bundle(num_accounts: usize) -> BundleState {
    let mut accounts = Vec::new();
    for i in 0..num_accounts {
        let addr = Address::from_word(B256::from(U256::from(i + 1)));
        let info = default_info(i as u64, 1000 + i as u64);
        accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, vec![]));
    }
    make_bundle(accounts)
}

/// I4.1: storage-heavy workload with default thresholds passes.
#[test]
fn i4_1_storage_heavy_default_thresholds() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    // 100 accounts with 10 storage slots each -> exceeds default storage_tries_min=64
    let bundle = storage_heavy_bundle(100, 10);
    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();
    assert_eq!(ver, 1);
    assert_ne!(root, alloy_trie::EMPTY_ROOT_HASH);
}

/// I4.2: account-heavy workload with default thresholds passes.
#[test]
fn i4_2_account_heavy_default_thresholds() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    // 200 accounts, no storage -> parallel account hash path if frontier is wide enough
    let bundle = account_heavy_bundle(200);
    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();
    assert_eq!(ver, 1);
    assert_ne!(root, alloy_trie::EMPTY_ROOT_HASH);
}

/// I4.3: close + reopen + load_version -> version and state_root match.
#[test]
fn i4_3_reopen_load_version_consistent() {
    let dir = TempDir::new().unwrap();

    let (version_before, root_before);
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit a storage-heavy block
        let bundle = storage_heavy_bundle(80, 8);
        store.apply_bundle_state(&bundle).unwrap();
        let result = store.commit().unwrap();
        version_before = result.0;
        root_before = result.1;
        store.close().unwrap();
    }

    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), version_before);

        // Commit empty block: root should stay the same
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root_after) = store.commit().unwrap();
        assert_eq!(ver, version_before + 1);
        assert_eq!(root_after, root_before);
    }
}
