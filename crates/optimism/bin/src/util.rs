use std::path::{Path, PathBuf};
use tempdir::TempDir;
use rand::prelude::*;
use rand::RngCore;
use alloy_primitives::{Address, StorageKey, StorageValue, U256, B256};
use reth_primitives_traits::{Account, StorageEntry};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use triedb::{
    account::Account as TrieDBAccount,
    path::{AddressPath, StoragePath},
    transaction::TransactionError,
    Database,
};
use std::{
    fs, io,
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};
use std::collections::HashMap;

pub const BATCH_SIZE: usize = 10_000;

pub fn generate_random_address(rng: &mut StdRng) -> AddressPath {
    let mut bytes = [0u8; 20];
    rng.fill_bytes(&mut bytes);
    let addr = Address::from_slice(&bytes);
    AddressPath::for_address(addr)
}

pub const DEFAULT_SETUP_DB_EOA_SIZE: usize = 1_000_000;
pub const DEFAULT_SETUP_DB_CONTRACT_SIZE: usize = 100_000;
pub const DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT: usize = 10;
pub const SEED_EOA: u64 = 42; // EOA seeding value
pub const SEED_CONTRACT: u64 = 43; // contract account seeding value


#[derive(Debug)]
#[allow(dead_code)]
pub struct FlatTrieDatabase {
    _base_dir: Option<TempDir>,
    pub main_file_name: String,
    pub file_name_path: PathBuf,
    pub meta_file_name: String,
    pub meta_file_name_path: PathBuf,
}
pub fn get_flat_trie_database(
    fallback_eoa_size: usize,
    fallback_contract_size: usize,
    fallback_storage_per_contract: usize,
    overlay_size: usize,
) -> (FlatTrieDatabase,(HashMap<Address, Account>, HashMap<Address, HashMap<B256, U256>>) ){

    let dir = TempDir::new("triedb_bench_base").unwrap();

    let main_file_name_path = dir.path().join("triedb");
    let meta_file_name_path = dir.path().join("triedb.meta");
    let db = Database::create_new(&main_file_name_path).unwrap();

    let (addresses, accounts_map, storage_map, overlay_acct, overlay_storage) =
        generate_shared_test_data(fallback_eoa_size, fallback_contract_size, fallback_storage_per_contract, overlay_size);

    let ret = setup_tdb_database(&db, &addresses, &accounts_map, &storage_map)
        .unwrap();

    (FlatTrieDatabase {
        _base_dir: Some(dir),
        main_file_name: "triedb".to_string(),
        file_name_path: main_file_name_path,
        meta_file_name: "triedb.meta".to_string(),
        meta_file_name_path,
    }, (overlay_acct, overlay_storage  ))
}
pub fn setup_tdb_database(
    db: &Database,
    addresses: &[Address],
    accounts_map: &HashMap<Address, Account>,
    storage_map: &HashMap<Address, HashMap<B256, U256>>,
) -> Result<(), TransactionError> {
    {
        let mut tx = db.begin_rw()?;

        // Set accounts from the provided data
        for address in addresses {
            if let Some(account) = accounts_map.get(address) {
                let address_path = AddressPath::for_address(*address);
                let trie_account = TrieDBAccount::new(
                    account.nonce,
                    account.balance,
                    EMPTY_ROOT_HASH,
                    account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
                );
                tx.set_account(address_path, Some(trie_account))?;
            }
        }

        // Set storage from the provided data (only for contracts)
        for (address, storage) in storage_map {
            let address_path = AddressPath::for_address(*address);
            for (storage_key, storage_value) in storage {
                let storage_path = StoragePath::for_address_path_and_slot(
                    address_path.clone(),
                    StorageKey::from(*storage_key),
                );
                // Fix: Use the actual storage value, not the slot
                let storage_value_triedb = StorageValue::from_be_slice(
                    storage_value.to_be_bytes::<32>().as_slice()
                );
                tx.set_storage_slot(storage_path, Some(storage_value_triedb))?;
            }
        }

        tx.commit()?;
    }

    Ok(())
}

// Helper function to generate shared test data using alloy primitives
pub fn generate_shared_test_data(
    eoa_count: usize,
    contract_count: usize,
    storage_per_contract: usize,
    overlay_count: usize, // total number of overlay addresses (can include duplicates and new ones)
) -> (
    Vec<Address>, // all base addresses (EOA + contracts)
    HashMap<Address, Account>, // base accounts map
    HashMap<Address, HashMap<B256, U256>>, // base storage map: address -> storage_key -> value
    HashMap<Address, Account>, // overlay accounts map (can have duplicates with base + new addresses)
    HashMap<Address, HashMap<B256, U256>>, // overlay storage map
) {
    let mut rng = StdRng::seed_from_u64(SEED_CONTRACT);

    // Generate EOA addresses
    let eoa_addresses: Vec<Address> = (0..eoa_count).map(|_| {
        let mut addr_bytes = [0u8; 20];
        rng.fill(&mut addr_bytes);
        Address::from_slice(&addr_bytes)
    }).collect();

    // Generate contract addresses
    let contract_addresses: Vec<Address> = (0..contract_count).map(|_| {
        let mut addr_bytes = [0u8; 20];
        rng.fill(&mut addr_bytes);
        Address::from_slice(&addr_bytes)
    }).collect();

    // Combine all base addresses
    let mut addresses = eoa_addresses.clone();
    addresses.extend(contract_addresses.clone());

    // Generate base accounts map
    let mut accounts_map = HashMap::new();
    for (i, address) in addresses.iter().enumerate() {
        let account = Account {
            nonce: i as u64,
            balance: U256::from(i as u64),
            bytecode_hash: if contract_addresses.contains(address) {
                // Contracts have bytecode hash
                Some(EMPTY_ROOT_HASH)
            } else {
                // EOAs have no bytecode
                None
            },
        };
        accounts_map.insert(*address, account);
    }

    // Generate base storage map (only for contracts)
    let mut storage_map: HashMap<Address, HashMap<B256, U256>> = HashMap::new();
    for address in &contract_addresses {
        let mut contract_storage = HashMap::new();
        for key in 1..=storage_per_contract {
            let storage_key = B256::from(U256::from(key));
            let storage_value = U256::from(key);
            contract_storage.insert(storage_key, storage_value);
        }
        storage_map.insert(*address, contract_storage);
    }

    // Generate overlay states
    // Some addresses can be duplicates (updates to existing), some can be new
    let mut overlay_accounts_map = HashMap::new();
    let mut overlay_storage_map: HashMap<Address, HashMap<B256, U256>> = HashMap::new();

    for i in 0..overlay_count {
        // Randomly decide: duplicate existing address or new address
        let is_existing = rng.gen_bool(0.5) && !addresses.is_empty();
        let address = if is_existing {
            // Update existing account (only storage, no account update)
            addresses[rng.gen_range(0..addresses.len())]
        } else {
            // Create new account
            let mut addr_bytes = [0u8; 20];
            rng.fill(&mut addr_bytes);
            Address::from_slice(&addr_bytes)
        };

        // Only generate overlay account for newly created accounts
        if !is_existing {
            // Generate overlay account (with different values)
            let overlay_account = Account {
                nonce: (i + 1000) as u64, // different nonce
                balance: U256::from((i + 2000) as u64), // different balance
                bytecode_hash: if rng.gen_bool(0.3) {
                    // 30% chance to be a contract
                    Some(EMPTY_ROOT_HASH)
                } else {
                    None
                },
            };
            overlay_accounts_map.insert(address, overlay_account);
        }

        // Generate overlay storage (only for contracts)
        // For existing addresses, check if they're contracts in base data
        // For new addresses, check if the overlay account is a contract
        let is_contract = if is_existing {
            // Check if existing address is a contract in base data
            accounts_map.get(&address)
                .map(|acc| acc.bytecode_hash.is_some())
                .unwrap_or(false)
        } else {
            // Check if new overlay account is a contract
            overlay_accounts_map.get(&address)
                .map(|acc| acc.bytecode_hash.is_some())
                .unwrap_or(false)
        };

        if is_contract {
            let mut contract_storage = HashMap::new();

            // Random number of storage changes (max half of storage_per_contract)
            let max_changes = (storage_per_contract / 2).max(1);
            let num_changes = rng.gen_range(1..=max_changes);

            // Get existing storage if this address exists in base storage_map
            let existing_storage = storage_map.get(&address);

            for _ in 0..num_changes {
                let change_type = rng.gen_range(0..3); // 0: new, 1: delete, 2: update

                match change_type {
                    0 => {
                        // New storage slot
                        let storage_key = B256::from(U256::from(rng.gen_range(1000..2000)));
                        let storage_value = U256::from(rng.gen_range(5000..10000));
                        contract_storage.insert(storage_key, storage_value);
                    }
                    1 => {
                        // Delete existing storage (value = 0)
                        if let Some(existing) = existing_storage {
                            if !existing.is_empty() {
                                let keys: Vec<B256> = existing.keys().copied().collect();
                                if !keys.is_empty() {
                                    let key_to_delete = keys[rng.gen_range(0..keys.len())];
                                    contract_storage.insert(key_to_delete, U256::ZERO);
                                }
                            }
                        }
                    }
                    2 => {
                        // Update existing storage
                        if let Some(existing) = existing_storage {
                            if !existing.is_empty() {
                                let keys: Vec<B256> = existing.keys().copied().collect();
                                if !keys.is_empty() {
                                    let key_to_update = keys[rng.gen_range(0..keys.len())];
                                    let new_value = U256::from(rng.gen_range(10000..20000));
                                    contract_storage.insert(key_to_update, new_value);
                                }
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }

            if !contract_storage.is_empty() {
                overlay_storage_map.insert(address, contract_storage);
            }
        }
    }

    (
        addresses,
        accounts_map,
        storage_map,
        overlay_accounts_map,
        overlay_storage_map,
    )
}

pub fn copy_files(from: &FlatTrieDatabase, to: &Path) -> Result<(), io::Error> {
    for (file, from_path) in [
        (&from.main_file_name, &from.file_name_path),
        (&from.meta_file_name, &from.meta_file_name_path),
    ] {
        let to_path = to.join(file);
        fs::copy(from_path, &to_path)?;
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_shared_test_data_single_eoa() {
        let (addresses, accounts_map, storage_map, overlay_accounts_map, overlay_storage_map) =
            generate_shared_test_data(1, 0, 0, 0);

        // Should have exactly 1 base address (EOA)
        assert_eq!(addresses.len(), 1, "Should have exactly 1 base address");

        // Should have exactly 1 account in base accounts map
        assert_eq!(accounts_map.len(), 1, "Should have exactly 1 account in base accounts map");

        // Verify the account properties
        let address = &addresses[0];
        let account = accounts_map.get(address).expect("Address should exist in accounts_map");
        assert_eq!(account.nonce, 0, "EOA should have nonce 0");
        assert_eq!(account.balance, U256::from(0), "EOA should have balance 0");
        assert_eq!(account.bytecode_hash, None, "EOA should have no bytecode hash");

        // Storage map should be empty (no contracts)
        assert!(storage_map.is_empty(), "Storage map should be empty when contract_count is 0");

        // Overlay maps should be empty (overlay_count is 0)
        assert!(overlay_accounts_map.is_empty(), "Overlay accounts map should be empty when overlay_count is 0");
        assert!(overlay_storage_map.is_empty(), "Overlay storage map should be empty when overlay_count is 0");
    }
    #[test]
    fn test_generate_shared_test_data_single_eoa_single_contract() {
        let (addresses, accounts_map, storage_map, overlay_accounts_map, overlay_storage_map) =
            generate_shared_test_data(1, 1, 0, 0);

        // Should have exactly 1 base address (EOA)
        assert_eq!(addresses.len(), 2, "Should have exactly 1 base address");

        // Should have exactly 1 account in base accounts map
        assert_eq!(accounts_map.len(), 2, "Should have exactly 1 account in base accounts map");


    }
}