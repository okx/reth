use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{HashBuilder, Nibbles, EMPTY_ROOT_HASH, KECCAK_EMPTY};
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

fn compute_storage_root(slots: &[(U256, U256)]) -> B256 {
    if slots.is_empty() {
        return EMPTY_ROOT_HASH;
    }
    let mut entries: Vec<(Nibbles, Vec<u8>)> = slots
        .iter()
        .filter(|(_, v)| *v != U256::ZERO)
        .map(|(k, v)| {
            let hashed = keccak256(k.to_be_bytes::<32>());
            let mut encoded = Vec::new();
            v.encode(&mut encoded);
            (Nibbles::unpack(hashed), encoded)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hb = HashBuilder::default();
    for (key, val) in entries {
        hb.add_leaf(key, &val);
    }
    hb.root()
}

fn compute_state_root(accounts: &[(Address, AccountInfo, B256)]) -> B256 {
    if accounts.is_empty() {
        return EMPTY_ROOT_HASH;
    }
    let mut entries: Vec<(Nibbles, Vec<u8>)> = accounts
        .iter()
        .map(|(addr, info, storage_root)| {
            let hashed = keccak256(addr);
            let ta = alloy_trie::TrieAccount {
                nonce: info.nonce,
                balance: info.balance,
                storage_root: *storage_root,
                code_hash: info.code_hash,
            };
            (Nibbles::unpack(hashed), alloy_rlp::encode(&ta))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hb = HashBuilder::default();
    for (key, val) in entries {
        hb.add_leaf(key, &val);
    }
    hb.root()
}

/// I7.1: block1 single account + single storage slot
#[test]
fn i7_1_single_account_single_slot() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x01);
    let info = default_info(1, 1000);
    let slot_key = U256::from(1);
    let slot_val = U256::from(42);

    let bundle = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::Changed,
        vec![(slot_key, U256::ZERO, slot_val)],
    )]);

    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();
    assert_eq!(ver, 1);

    let storage_root = compute_storage_root(&[(slot_key, slot_val)]);
    let expected = compute_state_root(&[(addr, info, storage_root)]);
    assert_eq!(root, expected);
}

/// I7.2: block2 only updates account fields, storage_root inherited
#[test]
fn i7_2_inherit_storage_root() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x02);
    let info1 = default_info(1, 100);
    let slot_key = U256::from(5);
    let slot_val = U256::from(99);

    // Block 1
    let bundle1 = make_bundle(vec![(
        addr,
        Some(info1),
        revm_database::AccountStatus::Changed,
        vec![(slot_key, U256::ZERO, slot_val)],
    )]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();

    // Block 2: only change balance
    let info2 = default_info(2, 200);
    let bundle2 = make_bundle(vec![(
        addr,
        Some(info2.clone()),
        revm_database::AccountStatus::Changed,
        vec![],
    )]);
    store.apply_bundle_state(&bundle2).unwrap();
    let (_, root2) = store.commit().unwrap();

    let storage_root = compute_storage_root(&[(slot_key, slot_val)]);
    let expected = compute_state_root(&[(addr, info2, storage_root)]);
    assert_eq!(root2, expected);
}

/// I7.3: block3 wipe + recreate
#[test]
fn i7_3_wipe_and_recreate() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x03);
    let info = default_info(1, 100);

    // Block 1: account with slot1
    let bundle1 = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::Changed,
        vec![(U256::from(1), U256::ZERO, U256::from(10))],
    )]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();

    // Block 2: destroy + recreate with new slot2
    let bundle2 = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::DestroyedChanged,
        vec![(U256::from(2), U256::ZERO, U256::from(20))],
    )]);
    store.apply_bundle_state(&bundle2).unwrap();
    let (_, root2) = store.commit().unwrap();

    // Expected: only slot2 (slot1 wiped)
    let storage_root = compute_storage_root(&[(U256::from(2), U256::from(20))]);
    let expected = compute_state_root(&[(addr, info, storage_root)]);
    assert_eq!(root2, expected);
}

/// I7.4: multiple accounts, multiple storage tries
#[test]
fn i7_4_multi_account_multi_storage() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr1 = Address::repeat_byte(0x10);
    let addr2 = Address::repeat_byte(0x20);
    let info1 = default_info(1, 100);
    let info2 = default_info(2, 200);

    let bundle = make_bundle(vec![
        (
            addr1,
            Some(info1.clone()),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        ),
        (
            addr2,
            Some(info2.clone()),
            revm_database::AccountStatus::Changed,
            vec![
                (U256::from(2), U256::ZERO, U256::from(22)),
                (U256::from(3), U256::ZERO, U256::from(33)),
            ],
        ),
    ]);

    store.apply_bundle_state(&bundle).unwrap();
    let (_, root) = store.commit().unwrap();

    let sr1 = compute_storage_root(&[(U256::from(1), U256::from(11))]);
    let sr2 =
        compute_storage_root(&[(U256::from(2), U256::from(22)), (U256::from(3), U256::from(33))]);
    let expected = compute_state_root(&[(addr1, info1, sr1), (addr2, info2, sr2)]);
    assert_eq!(root, expected);
}

/// I7.5: 3 blocks consecutive commit, version monotonically increases
#[test]
fn i7_5_three_blocks() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    for i in 1..=3 {
        let addr = Address::repeat_byte(i as u8);
        let info = default_info(i, i * 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, _) = store.commit().unwrap();
        assert_eq!(ver, i as i64);
    }
}

/// I7.6: reopen after close -> load_version, state_root unchanged
#[test]
fn i7_6_reopen_load() {
    let dir = TempDir::new().unwrap();

    let root1;
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x50);
        let info = default_info(1, 500);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(100))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, r) = store.commit().unwrap();
        root1 = r;
        store.close().unwrap();
    }

    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 1);
        // Commit empty block to verify state is intact
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root2) = store.commit().unwrap();
        assert_eq!(ver, 2);
        // root2 should be same as root1 since no changes to the account
        assert_eq!(root2, root1);
    }
}

/// I7.7: rollback to old version, reopen, state_root consistent
#[test]
fn i7_7_rollback_and_reopen() {
    let dir = TempDir::new().unwrap();

    let root1;
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Block 1: create account
        let addr = Address::repeat_byte(0x60);
        let info1 = default_info(1, 100);
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, r1) = store.commit().unwrap();
        root1 = r1;

        // Block 2: update account
        let info2 = default_info(2, 200);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();

        // Rollback to version 1
        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        store.close().unwrap();
    }

    // Reopen and verify
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 1);

        // Verify by committing empty block: should get root consistent with version 1 state
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root_after) = store.commit().unwrap();
        assert_eq!(ver, 2);
        // root should equal root1 since no state changes (empty block)
        assert_eq!(root_after, root1);
    }
}
