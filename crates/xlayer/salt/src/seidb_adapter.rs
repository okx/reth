//! Conversion from revm `BundleState` to sei-db `NamedChangeSet` format.
//!
//! Maps EVM execution results into sei-db's store-based changeset model:
//! - Account balance/nonce changes -> "bank" store KV pairs
//! - Storage slot changes -> "evm" store KV pairs (with 0x03 prefix for storage)

use alloy_primitives::Address;
use revm_database::BundleState;
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};

const BANK_STORE: &str = "bank";
const EVM_STORE: &str = "evm";

/// Prefix byte for storage keys in the EVM store, matching sei-chain convention.
const STATE_KEY_PREFIX: u8 = 0x03;

/// Convert a `BundleState` (from EVM execution) into sei-db `NamedChangeSet`s.
///
/// Maps:
///   - Account balance/nonce changes -> "bank" store KV pairs
///   - Storage slot changes -> "evm" store KV pairs (with 0x03 prefix)
pub fn bundle_to_changesets(bundle: &BundleState) -> Vec<NamedChangeSet> {
    let mut bank_pairs = Vec::new();
    let mut evm_pairs = Vec::new();

    for (addr, account) in bundle.state() {
        let addr = Address::from(*addr);

        // Account data -> bank store
        if let Some(info) = &account.info {
            let key = addr.as_slice().to_vec();
            let mut value = Vec::with_capacity(40);
            value.extend_from_slice(&info.balance.to_be_bytes::<32>());
            value.extend_from_slice(&info.nonce.to_be_bytes());

            let is_destroyed = account.was_destroyed();
            bank_pairs.push(KvPair {
                delete: is_destroyed,
                key,
                value: if is_destroyed { vec![] } else { value },
            });
        } else if account.was_destroyed() {
            // Account destroyed with no info -- emit a delete
            bank_pairs.push(KvPair { delete: true, key: addr.as_slice().to_vec(), value: vec![] });
        }

        // Storage changes -> evm store (with 0x03 prefix)
        for (slot, slot_info) in &account.storage {
            let mut key = Vec::with_capacity(1 + 20 + 32);
            key.push(STATE_KEY_PREFIX);
            key.extend_from_slice(addr.as_slice()); // 20 bytes
            key.extend_from_slice(&slot.to_be_bytes::<32>()); // 32 bytes

            let is_zero = slot_info.present_value.is_zero();
            evm_pairs.push(KvPair {
                delete: is_zero,
                key,
                value: if is_zero {
                    vec![]
                } else {
                    slot_info.present_value.to_be_bytes::<32>().to_vec()
                },
            });
        }
    }

    let mut result = Vec::new();
    if !bank_pairs.is_empty() {
        result.push(NamedChangeSet {
            name: BANK_STORE.into(),
            changeset: Some(ChangeSet { pairs: bank_pairs }),
        });
    }
    if !evm_pairs.is_empty() {
        result.push(NamedChangeSet {
            name: EVM_STORE.into(),
            changeset: Some(ChangeSet { pairs: evm_pairs }),
        });
    }
    result
}

/// Pre-populate sei-db with account data from a BundleState using OP_CREATE semantics.
/// All accounts go to "bank", all storage slots go to "evm".
/// Uses chunked commits to avoid overwhelming memiavl with huge changesets.
pub fn bundle_to_pre_populate_changesets(
    bundle: &BundleState,
    chunk_size: usize,
) -> Vec<Vec<NamedChangeSet>> {
    let accounts: Vec<_> = bundle.state().iter().collect();
    let mut chunks = Vec::new();

    for chunk in accounts.chunks(chunk_size) {
        let mut bank_pairs = Vec::new();
        let mut evm_pairs = Vec::new();

        for (addr, account) in chunk {
            let addr = Address::from(**addr);

            if let Some(info) = &account.info {
                let key = addr.as_slice().to_vec();
                let mut value = Vec::with_capacity(40);
                value.extend_from_slice(&info.balance.to_be_bytes::<32>());
                value.extend_from_slice(&info.nonce.to_be_bytes());
                bank_pairs.push(KvPair { delete: false, key, value });
            }

            for (slot, slot_info) in &account.storage {
                let mut key = Vec::with_capacity(1 + 20 + 32);
                key.push(STATE_KEY_PREFIX);
                key.extend_from_slice(addr.as_slice());
                key.extend_from_slice(&slot.to_be_bytes::<32>());

                let is_zero = slot_info.present_value.is_zero();
                evm_pairs.push(KvPair {
                    delete: is_zero,
                    key,
                    value: if is_zero {
                        vec![]
                    } else {
                        slot_info.present_value.to_be_bytes::<32>().to_vec()
                    },
                });
            }
        }

        let mut changesets = Vec::new();
        if !bank_pairs.is_empty() {
            changesets.push(NamedChangeSet {
                name: BANK_STORE.into(),
                changeset: Some(ChangeSet { pairs: bank_pairs }),
            });
        }
        if !evm_pairs.is_empty() {
            changesets.push(NamedChangeSet {
                name: EVM_STORE.into(),
                changeset: Some(ChangeSet { pairs: evm_pairs }),
            });
        }
        if !changesets.is_empty() {
            chunks.push(changesets);
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::constants::KECCAK_EMPTY;
    use alloy_primitives::{map::HashMap as PrimitivesHashMap, B256, U256};
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
    use revm_state::AccountInfo;

    fn make_bundle(
        accounts: Vec<(Address, Option<AccountInfo>, Vec<(B256, U256)>)>,
    ) -> BundleState {
        let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
            PrimitivesHashMap::default();

        for (addr, info, slots) in accounts {
            let mut storage = StorageWithOriginalValues::default();
            for (slot_key, value) in slots {
                storage.insert(
                    U256::from_be_bytes(slot_key.0),
                    StorageSlot::new_changed(U256::ZERO, value),
                );
            }
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info,
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage,
                },
            );
        }

        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    #[test]
    fn test_bundle_to_changesets_basic() {
        let addr = Address::from([0x01; 20]);
        let info = AccountInfo {
            nonce: 5,
            balance: U256::from(1000u64),
            code_hash: KECCAK_EMPTY,
            code: None,
            account_id: None,
        };
        let slot = B256::from([0xaa; 32]);
        let value = U256::from(42u64);

        let bundle = make_bundle(vec![(addr, Some(info), vec![(slot, value)])]);
        let changesets = bundle_to_changesets(&bundle);

        // Should have both bank and evm changesets
        assert_eq!(changesets.len(), 2);

        let bank = changesets.iter().find(|c| c.name == BANK_STORE).unwrap();
        let bank_pairs = &bank.changeset.as_ref().unwrap().pairs;
        assert_eq!(bank_pairs.len(), 1);
        assert!(!bank_pairs[0].delete);
        assert_eq!(bank_pairs[0].key, addr.as_slice());

        let evm = changesets.iter().find(|c| c.name == EVM_STORE).unwrap();
        let evm_pairs = &evm.changeset.as_ref().unwrap().pairs;
        assert_eq!(evm_pairs.len(), 1);
        assert!(!evm_pairs[0].delete);
        assert_eq!(evm_pairs[0].key[0], STATE_KEY_PREFIX);
    }

    #[test]
    fn test_bundle_to_changesets_zero_storage_is_delete() {
        let addr = Address::from([0x02; 20]);
        let info = AccountInfo {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK_EMPTY,
            code: None,
            account_id: None,
        };
        let slot = B256::from([0xbb; 32]);

        let bundle = make_bundle(vec![(addr, Some(info), vec![(slot, U256::ZERO)])]);
        let changesets = bundle_to_changesets(&bundle);

        let evm = changesets.iter().find(|c| c.name == EVM_STORE).unwrap();
        let evm_pairs = &evm.changeset.as_ref().unwrap().pairs;
        assert!(evm_pairs[0].delete);
        assert!(evm_pairs[0].value.is_empty());
    }

    #[test]
    fn test_bundle_to_pre_populate_chunking() {
        let mut accounts = Vec::new();
        for i in 0..5u8 {
            let addr = Address::from([i + 1; 20]);
            let info = AccountInfo {
                nonce: i as u64,
                balance: U256::from(1000u64 * (i as u64 + 1)),
                code_hash: KECCAK_EMPTY,
                code: None,
                account_id: None,
            };
            accounts.push((addr, Some(info), vec![]));
        }
        let bundle = make_bundle(accounts);

        let chunks = bundle_to_pre_populate_changesets(&bundle, 2);
        // 5 accounts with chunk_size=2 -> 3 chunks (2, 2, 1)
        assert_eq!(chunks.len(), 3);
    }
}
