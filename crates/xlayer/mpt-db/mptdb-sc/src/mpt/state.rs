use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_trie::Nibbles;
use mptdb_common::error::{MptDbError, Result};
use rayon::prelude::*;
use revm_database::BundleState;
use revm_state::AccountInfo;

/// Final storage slot override for a single account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageChange {
    pub hashed_slot: B256,
    pub slot_key: Nibbles,
    pub value: U256,
    pub encoded_value: Option<Vec<u8>>,
}

/// Represents a single account's dirty state for one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtyAccount {
    /// Original address (for debugging/testing); trie key uses keccak256(address).
    pub address: Address,
    pub hashed_address: B256,
    pub account_key: Nibbles,

    /// Final account info after block execution; None means account does not exist.
    pub info: Option<AccountInfo>,

    /// Whether the account's storage was fully wiped (e.g., selfdestruct).
    pub storage_wiped: bool,
    /// Whether the source BundleAccount guarantees storage starts from an empty baseline.
    pub storage_known_empty: bool,

    /// Final storage slot overrides (including U256::ZERO = delete slot).
    /// Key = keccak256(slot_key).
    pub storage_changes: Vec<StorageChange>,
}

#[derive(Clone, Copy)]
enum CollectMode {
    Standard,
    PrePop,
}

/// Convert a BundleState into sorted, deduplicated DirtyAccounts.
///
/// Output is sorted by `hashed_address` for deterministic commit ordering.
pub fn collect_dirty_accounts(bundle: &BundleState) -> Result<Vec<DirtyAccount>> {
    collect_accounts(bundle, CollectMode::Standard)
}

/// Convert a fresh-DB pre-pop BundleState into sorted DirtyAccounts.
///
/// This path assumes every account starts from an empty storage baseline and rejects
/// delete/selfdestruct semantics that only make sense for normal block execution.
pub fn collect_prepop_accounts(bundle: &BundleState) -> Result<Vec<DirtyAccount>> {
    collect_accounts(bundle, CollectMode::PrePop)
}

fn collect_accounts(bundle: &BundleState, mode: CollectMode) -> Result<Vec<DirtyAccount>> {
    let mut accounts: Vec<DirtyAccount> = bundle
        .state
        .iter()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(address, bundle_account)| -> Result<DirtyAccount> {
            let hashed_address = keccak256(address);
            let account_key = Nibbles::unpack(&hashed_address);

            let (info, storage_wiped, storage_known_empty) = match mode {
                CollectMode::Standard => {
                    let info = bundle_account.info.clone();
                    let storage_wiped = bundle_account.was_destroyed();
                    let storage_known_empty =
                        bundle_account.status.is_storage_known() && !storage_wiped;
                    (info, storage_wiped, storage_known_empty)
                }
                CollectMode::PrePop => {
                    if bundle_account.was_destroyed() || bundle_account.info.is_none() {
                        return Err(MptDbError::Other(
                            "bulk pre-pop only supports live account inserts".to_string(),
                        ));
                    }
                    (bundle_account.info.clone(), false, true)
                }
            };

            let storage_changes = bundle_account
                .storage
                .iter()
                .map(|(slot_key, slot)| {
                    let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
                    StorageChange {
                        hashed_slot,
                        slot_key: Nibbles::unpack(&hashed_slot),
                        value: slot.present_value,
                        encoded_value: (slot.present_value != U256::ZERO)
                            .then(|| alloy_rlp::encode(slot.present_value)),
                    }
                })
                .collect();

            Ok(DirtyAccount {
                address: *address,
                hashed_address,
                account_key,
                info,
                storage_wiped,
                storage_known_empty,
                storage_changes,
            })
        })
        .collect::<Result<_>>()?;

    // Sort by hashed_address for deterministic ordering.
    // par_sort_unstable_by uses rayon to sort in parallel; safe because
    // DirtyAccount: Send and the comparator is pure.
    accounts.par_sort_unstable_by(|a, b| a.hashed_address.cmp(&b.hashed_address));
    // Dedup (shouldn't happen with HashMap input, but enforce contract)
    accounts.dedup_by(|a, b| a.hashed_address == b.hashed_address);

    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_database::{states::StorageSlot, BundleAccount};

    fn make_info(nonce: u64, balance: U256, code_hash: B256) -> AccountInfo {
        AccountInfo { nonce, balance, code_hash, account_id: None, code: None }
    }

    fn make_bundle_with_account(
        address: Address,
        info: Option<AccountInfo>,
        status: revm_database::AccountStatus,
        storage: Vec<(U256, U256, U256)>, // (key, original, present)
    ) -> BundleState {
        let storage_map: revm_database::StorageWithOriginalValues = storage
            .into_iter()
            .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
            .collect();

        let bundle_account = BundleAccount::new(
            None, // original_info
            info,
            storage_map,
            status,
        );

        let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
            alloy_primitives::map::HashMap::default();
        state.insert(address, bundle_account);

        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    /// T2.1: empty BundleState -> empty dirty accounts
    #[test]
    fn t2_1_empty_bundle() {
        let bundle = BundleState::default();
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert!(result.is_empty());
    }

    /// T2.2: single account nonce/balance/code_hash update
    #[test]
    fn t2_2_single_account_update() {
        let addr = Address::repeat_byte(0x01);
        let info = make_info(5, U256::from(1000), B256::repeat_byte(0xab));
        let bundle = make_bundle_with_account(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![],
        );
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].address, addr);
        assert_eq!(result[0].hashed_address, keccak256(addr));
        assert_eq!(result[0].info.as_ref().unwrap().nonce, 5);
        assert!(!result[0].storage_wiped);
        assert!(result[0].storage_changes.is_empty());
    }

    /// T2.3: storage slot non-zero update
    #[test]
    fn t2_3_storage_nonzero_update() {
        let addr = Address::repeat_byte(0x02);
        let info = make_info(1, U256::from(100), B256::repeat_byte(0xcc));
        let slot_key = U256::from(42);
        let bundle = make_bundle_with_account(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::ZERO, U256::from(999))],
        );
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result.len(), 1);
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let change = result[0]
            .storage_changes
            .iter()
            .find(|change| change.hashed_slot == hashed_slot)
            .unwrap();
        assert_eq!(change.value, U256::from(999));
    }

    /// T2.4: storage slot = ZERO is preserved (commit decides delete)
    #[test]
    fn t2_4_storage_zero_preserved() {
        let addr = Address::repeat_byte(0x03);
        let info = make_info(1, U256::from(50), B256::repeat_byte(0xdd));
        let slot_key = U256::from(7);
        let bundle = make_bundle_with_account(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::from(100), U256::ZERO)],
        );
        let result = collect_dirty_accounts(&bundle).unwrap();
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let change = result[0]
            .storage_changes
            .iter()
            .find(|change| change.hashed_slot == hashed_slot)
            .unwrap();
        assert_eq!(change.value, U256::ZERO);
    }

    /// T2.5: selfdestruct without rebuild -> info=None, storage_wiped=true
    #[test]
    fn t2_5_selfdestruct_no_rebuild() {
        let addr = Address::repeat_byte(0x04);
        let bundle =
            make_bundle_with_account(addr, None, revm_database::AccountStatus::Destroyed, vec![]);
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].info.is_none());
        assert!(result[0].storage_wiped);
    }

    /// T2.6: selfdestruct then rebuild -> info=Some, storage_wiped=true
    #[test]
    fn t2_6_selfdestruct_then_rebuild() {
        let addr = Address::repeat_byte(0x05);
        let info = make_info(0, U256::from(500), B256::repeat_byte(0xee));
        let bundle = make_bundle_with_account(
            addr,
            Some(info),
            revm_database::AccountStatus::DestroyedChanged,
            vec![],
        );
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].info.is_some());
        assert!(result[0].storage_wiped);
    }

    /// T2.7: output sorted by hashed_address
    #[test]
    fn t2_7_sorted_by_hashed_address() {
        let addr1 = Address::repeat_byte(0x10);
        let addr2 = Address::repeat_byte(0x20);
        let info = make_info(1, U256::from(100), B256::repeat_byte(0xaa));

        let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
            alloy_primitives::map::HashMap::default();
        state.insert(
            addr1,
            BundleAccount::new(
                None,
                Some(info.clone()),
                Default::default(),
                revm_database::AccountStatus::Changed,
            ),
        );
        state.insert(
            addr2,
            BundleAccount::new(
                None,
                Some(info),
                Default::default(),
                revm_database::AccountStatus::Changed,
            ),
        );

        let bundle = BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        };

        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].hashed_address < result[1].hashed_address);
    }

    /// T2.8: hashed addresses match keccak256(address)
    #[test]
    fn t2_8_hashed_address_matches_keccak() {
        let addr = Address::repeat_byte(0xab);
        let info = make_info(0, U256::ZERO, B256::repeat_byte(0xff));
        let bundle = make_bundle_with_account(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![],
        );
        let result = collect_dirty_accounts(&bundle).unwrap();
        assert_eq!(result[0].hashed_address, keccak256(addr));
    }
}
