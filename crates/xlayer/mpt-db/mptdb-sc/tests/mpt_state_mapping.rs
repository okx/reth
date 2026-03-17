//! Integration tests for BundleState → DirtyAccount mapping (Phase 2 I6).
//!
//! These tests exercise `collect_dirty_accounts` end-to-end with realistic
//! BundleState inputs and cross-check against reth's `HashedPostState`.

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_trie::KECCAK_EMPTY;
use mptdb_sc::mpt::state::collect_dirty_accounts;
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;

fn storage_value(account: &mptdb_sc::mpt::state::DirtyAccount, hashed_slot: B256) -> Option<U256> {
    account
        .storage_changes
        .iter()
        .find(|change| change.hashed_slot == hashed_slot)
        .map(|change| change.value)
}

fn make_info(nonce: u64, balance: u64) -> AccountInfo {
    AccountInfo {
        nonce,
        balance: U256::from(balance),
        code_hash: KECCAK_EMPTY,
        account_id: None,
        code: None,
    }
}

fn make_bundle(
    accounts: Vec<(Address, Option<AccountInfo>, AccountStatus, Vec<(U256, U256, U256)>)>,
) -> BundleState {
    let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
        alloy_primitives::map::HashMap::default();
    for (addr, info, status, storage) in accounts {
        let storage_map: revm_database::StorageWithOriginalValues = storage
            .into_iter()
            .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
            .collect();
        state.insert(addr, BundleAccount::new(None, info, storage_map, status));
    }
    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

/// I6.1: BundleState → DirtyAccount basic mapping.
///
/// Multi-account bundle with mixed fields → each DirtyAccount has correct
/// hashed_address, info, storage_wiped=false, hashed storage keys.
#[test]
fn i6_1_basic_mapping() {
    let addr1 = Address::with_last_byte(0x01);
    let addr2 = Address::with_last_byte(0x02);
    let info1 = make_info(1, 1000);
    let info2 = make_info(5, 500);

    let bundle = make_bundle(vec![
        (
            addr1,
            Some(info1.clone()),
            AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(42))],
        ),
        (addr2, Some(info2.clone()), AccountStatus::Changed, vec![]),
    ]);

    let dirty = collect_dirty_accounts(&bundle).unwrap();
    assert_eq!(dirty.len(), 2);

    // Sorted by hashed_address
    assert!(dirty[0].hashed_address < dirty[1].hashed_address);

    // Find addr1's entry
    let d1 = dirty.iter().find(|d| d.address == addr1).unwrap();
    assert_eq!(d1.hashed_address, keccak256(addr1));
    assert_eq!(d1.info.as_ref().unwrap().nonce, 1);
    assert!(!d1.storage_wiped);
    // Storage key is keccak256(slot_key_be_bytes)
    let expected_hashed_slot = keccak256(U256::from(1).to_be_bytes::<32>());
    assert_eq!(storage_value(d1, expected_hashed_slot).unwrap(), U256::from(42));

    // Find addr2's entry
    let d2 = dirty.iter().find(|d| d.address == addr2).unwrap();
    assert_eq!(d2.hashed_address, keccak256(addr2));
    assert!(d2.storage_changes.is_empty());
    assert!(!d2.storage_wiped);
}

/// I6.2: destroyed then recreated mapping.
///
/// Account with DestroyedChanged status + new info + new storage slots →
/// storage_wiped=true, info=Some(new_info), storage_changes has new slots.
#[test]
fn i6_2_destroyed_then_recreated() {
    let addr = Address::with_last_byte(0x10);
    let new_info = make_info(0, 999);

    let bundle = make_bundle(vec![(
        addr,
        Some(new_info.clone()),
        AccountStatus::DestroyedChanged,
        vec![
            (U256::from(100), U256::ZERO, U256::from(200)),
            (U256::from(101), U256::ZERO, U256::from(201)),
        ],
    )]);

    let dirty = collect_dirty_accounts(&bundle).unwrap();
    assert_eq!(dirty.len(), 1);

    let d = &dirty[0];
    assert_eq!(d.address, addr);
    // DestroyedChanged → storage_wiped=true (was_destroyed() returns true)
    assert!(d.storage_wiped);
    // But info is Some (account was recreated)
    assert!(d.info.is_some());
    assert_eq!(d.info.as_ref().unwrap().balance, U256::from(999));
    // New storage slots present
    assert_eq!(d.storage_changes.len(), 2);
    let h100 = keccak256(U256::from(100).to_be_bytes::<32>());
    let h101 = keccak256(U256::from(101).to_be_bytes::<32>());
    assert_eq!(storage_value(d, h100).unwrap(), U256::from(200));
    assert_eq!(storage_value(d, h101).unwrap(), U256::from(201));
}

/// I6.3: wiped + ZERO slots mapping.
///
/// Account destroyed with a slot set to ZERO → storage_wiped=true,
/// storage_changes contains the ZERO value (commit layer decides to delete).
#[test]
fn i6_3_wiped_with_zero_slots() {
    let addr = Address::with_last_byte(0x20);
    let info = make_info(1, 100);

    let bundle = make_bundle(vec![(
        addr,
        Some(info),
        AccountStatus::DestroyedChanged,
        vec![
            (U256::from(5), U256::from(50), U256::ZERO), // slot deleted
            (U256::from(6), U256::ZERO, U256::from(60)), // slot created
        ],
    )]);

    let dirty = collect_dirty_accounts(&bundle).unwrap();
    assert_eq!(dirty.len(), 1);

    let d = &dirty[0];
    assert!(d.storage_wiped);
    assert_eq!(d.storage_changes.len(), 2);

    let h5 = keccak256(U256::from(5).to_be_bytes::<32>());
    let h6 = keccak256(U256::from(6).to_be_bytes::<32>());
    // ZERO is preserved in storage_changes — commit layer will interpret as delete
    assert_eq!(storage_value(d, h5).unwrap(), U256::ZERO);
    assert_eq!(storage_value(d, h6).unwrap(), U256::from(60));
}

/// I6.4: alignment with reth's HashedPostState::from_bundle_state.
///
/// For the same BundleState input, verify that our collect_dirty_accounts
/// produces the same set of hashed addresses and hashed storage keys as
/// reth's HashedPostState::from_bundle_state::<KeccakKeyHasher>.
#[test]
fn i6_4_alignment_with_hashed_post_state() {
    use reth_trie::{HashedPostState, KeccakKeyHasher};

    let addr1 = Address::with_last_byte(0x30);
    let addr2 = Address::with_last_byte(0x31);
    let info1 = make_info(3, 3000);
    let info2 = make_info(7, 7000);

    let bundle = make_bundle(vec![
        (
            addr1,
            Some(info1),
            AccountStatus::Changed,
            vec![
                (U256::from(10), U256::ZERO, U256::from(100)),
                (U256::from(20), U256::ZERO, U256::from(200)),
            ],
        ),
        (
            addr2,
            Some(info2),
            AccountStatus::Changed,
            vec![(U256::from(30), U256::from(300), U256::ZERO)],
        ),
    ]);

    // Our implementation
    let dirty = collect_dirty_accounts(&bundle).unwrap();

    // reth reference
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());

    // Compare: same set of hashed addresses
    let our_addresses: std::collections::HashSet<B256> =
        dirty.iter().map(|d| d.hashed_address).collect();
    let reth_addresses: std::collections::HashSet<B256> = hashed.accounts.keys().copied().collect();
    assert_eq!(our_addresses, reth_addresses, "hashed address sets must match");

    // Compare: for each account, same set of hashed storage keys
    for d in &dirty {
        let reth_storage = hashed.storages.get(&d.hashed_address);
        let our_keys: std::collections::HashSet<B256> =
            d.storage_changes.iter().map(|change| change.hashed_slot).collect();

        match reth_storage {
            Some(storage) => {
                let reth_keys: std::collections::HashSet<B256> =
                    storage.storage.keys().copied().collect();
                assert_eq!(
                    our_keys, reth_keys,
                    "hashed storage keys must match for address {:?}",
                    d.address
                );
            }
            None => {
                assert!(our_keys.is_empty(), "reth has no storage but we do for {:?}", d.address);
            }
        }
    }
}
