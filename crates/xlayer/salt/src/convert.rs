//! Conversion from revm `BundleState` to SALT plain key-value updates.
//!
//! After EVM execution produces a `BundleState`, this module converts the account
//! and storage changes into SALT's plain key-value format for state root computation.

use crate::account::{account_plain_key, encode_account, encode_storage_value, storage_plain_key};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Address, U256};
use revm_database::BundleAccount;
use std::collections::HashMap;

/// Convert a revm `BundleState` into SALT plain key-value updates.
///
/// Returns a `HashMap<Vec<u8>, Option<Vec<u8>>>` where:
/// - Key = SALT plain key (20 bytes for account, 52 bytes for storage)
/// - Value = `Some(encoded_bytes)` for upsert, `None` for deletion
///
/// This output can be fed directly into `EphemeralSaltState::update_fin()`.
pub fn bundle_state_to_plain_kv(
    state: &revm_database::BundleState,
) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
    let mut kvs: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();

    for (address, bundle_account) in state.state() {
        let address = Address::from(*address);
        process_account(&mut kvs, &address, bundle_account);
    }

    kvs
}

fn process_account(
    kvs: &mut HashMap<Vec<u8>, Option<Vec<u8>>>,
    address: &Address,
    bundle_account: &BundleAccount,
) {
    let was_destroyed = bundle_account.was_destroyed();

    if was_destroyed {
        kvs.insert(account_plain_key(address), None);
    } else if bundle_account.is_info_changed() {
        if let Some(info) = &bundle_account.info {
            let code_hash =
                if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
            let account = reth_primitives_traits::Account {
                nonce: info.nonce,
                balance: info.balance,
                bytecode_hash: code_hash,
            };
            kvs.insert(account_plain_key(address), Some(encode_account(&account)));
        } else {
            kvs.insert(account_plain_key(address), None);
        }
    }

    for (slot, slot_info) in &bundle_account.storage {
        let slot_b256 = alloy_primitives::B256::from(*slot);
        let key = storage_plain_key(address, &slot_b256);

        // Destroyed account: delete all storage slots regardless of present_value.
        if was_destroyed {
            kvs.insert(key, None);
            continue;
        }

        let changed = slot_info.is_changed();
        if changed {
            if slot_info.present_value == U256::ZERO {
                kvs.insert(key, None);
            } else {
                kvs.insert(key, Some(encode_storage_value(&slot_info.present_value)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{map::HashMap as PrimitivesHashMap, B256};
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
    use revm_state::AccountInfo;

    fn make_account_info(nonce: u64, balance: U256, code_hash: B256) -> AccountInfo {
        AccountInfo { nonce, balance, code_hash, account_id: None, code: None }
    }

    fn make_bundle_state(
        accounts: Vec<(Address, AccountStatus, Option<AccountInfo>, Vec<(B256, U256)>)>,
    ) -> revm_database::BundleState {
        let mut state = PrimitivesHashMap::default();

        for (addr, status, info, slots) in accounts {
            let mut storage = StorageWithOriginalValues::default();
            for (slot_key, value) in slots {
                // Use non-zero original so is_changed() == true even when present value is zero.
                storage.insert(slot_key.into(), StorageSlot::new_changed(U256::ONE, value));
            }
            state.insert(
                addr,
                BundleAccount { info: info.clone(), original_info: None, storage, status },
            );
        }

        revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    #[test]
    fn test_convert_account_change() {
        let addr = Address::from([0x01; 20]);
        let info = make_account_info(5, U256::from(1000u64), KECCAK_EMPTY);

        let bundle = make_bundle_state(vec![(addr, AccountStatus::Changed, Some(info), vec![])]);
        let kvs = bundle_state_to_plain_kv(&bundle);

        let key = account_plain_key(&addr);
        assert!(kvs.contains_key(&key));
        assert!(kvs[&key].is_some());
    }

    #[test]
    fn test_convert_storage_change() {
        let addr = Address::from([0x02; 20]);
        let info = make_account_info(0, U256::ZERO, KECCAK_EMPTY);
        let slot = B256::from([0xaa; 32]);
        let value = U256::from(42u64);

        let bundle = make_bundle_state(vec![(
            addr,
            AccountStatus::Changed,
            Some(info),
            vec![(slot, value)],
        )]);
        let kvs = bundle_state_to_plain_kv(&bundle);

        let storage_key = storage_plain_key(&addr, &slot);
        assert!(kvs.contains_key(&storage_key));
        assert!(kvs[&storage_key].is_some());
    }

    #[test]
    fn test_convert_destroyed_account() {
        let addr = Address::from([0x03; 20]);
        let bundle = make_bundle_state(vec![(addr, AccountStatus::Destroyed, None, vec![])]);
        let kvs = bundle_state_to_plain_kv(&bundle);

        let key = account_plain_key(&addr);
        assert_eq!(kvs[&key], None);
    }

    #[test]
    fn test_convert_zero_storage_is_deletion() {
        let addr = Address::from([0x04; 20]);
        let info = make_account_info(0, U256::ZERO, KECCAK_EMPTY);
        let slot = B256::from([0xbb; 32]);

        let bundle = make_bundle_state(vec![(
            addr,
            AccountStatus::Changed,
            Some(info),
            vec![(slot, U256::ZERO)],
        )]);
        let kvs = bundle_state_to_plain_kv(&bundle);

        let storage_key = storage_plain_key(&addr, &slot);
        assert_eq!(kvs[&storage_key], None);
    }
}
