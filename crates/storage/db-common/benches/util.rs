use std::path::{Path, PathBuf};
use tempdir::TempDir;
use rand::prelude::*;
use rand::RngCore;
use alloy_primitives::{Address, StorageKey, StorageValue, U256};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use triedb::{
    account::Account,
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
) -> FlatTrieDatabase {
    let base_dir = std::env::var("BASE_DIR").ok();
    if let Some(base_dir) = base_dir {
        let file_name =
            std::env::var("FILE_NAME").expect("FILE_NAME must be set when using BASE_DIR");
        let main_file_name = file_name.to_string();
        let meta_file_name = format!("{file_name}.meta");
        let file_name_path = Path::new(&base_dir).join(&main_file_name);
        let meta_file_name_path = Path::new(&base_dir).join(&meta_file_name);

        return FlatTrieDatabase {
            _base_dir: None,
            main_file_name,
            meta_file_name,
            file_name_path,
            meta_file_name_path,
        };
    }
    let dir = TempDir::new("triedb_bench_base").unwrap();

    let main_file_name_path = dir.path().join("triedb");
    let meta_file_name_path = dir.path().join("triedb.meta");
    let db = Database::create_new(&main_file_name_path).unwrap();

    setup_database(&db, fallback_eoa_size, fallback_contract_size, fallback_storage_per_contract)
        .unwrap();

    FlatTrieDatabase {
        _base_dir: Some(dir),
        main_file_name: "triedb".to_string(),
        file_name_path: main_file_name_path,
        meta_file_name: "triedb.meta".to_string(),
        meta_file_name_path,
    }
}

fn setup_database(
    db: &Database,
    eoa_count: usize,
    contract_count: usize,
    storage_per_contract: usize,
) -> Result<(), TransactionError> {
    // Populate database with initial accounts
    let mut eoa_rng = StdRng::seed_from_u64(SEED_EOA);
    let mut contract_rng = StdRng::seed_from_u64(SEED_CONTRACT);
    {
        let mut tx = db.begin_rw()?;
        for i in 1..=eoa_count {
            let address = generate_random_address(&mut eoa_rng);
            let account =
                Account::new(i as u64, U256::from(i as u64), EMPTY_ROOT_HASH, KECCAK_EMPTY);

            tx.set_account(address, Some(account))?;
        }

        for i in 1..=contract_count {
            let address = generate_random_address(&mut contract_rng);
            let account =
                Account::new(i as u64, U256::from(i as u64), EMPTY_ROOT_HASH, KECCAK_EMPTY);

            tx.set_account(address.clone(), Some(account))?;

            // add random storage to each account
            for key in 1..=storage_per_contract {
                let storage_key = StorageKey::from(U256::from(key));
                let storage_path =
                    StoragePath::for_address_path_and_slot(address.clone(), storage_key);
                let storage_value =
                    StorageValue::from_be_slice(storage_path.get_slot().pack().as_slice());

                tx.set_storage_slot(storage_path, Some(storage_value))?;
            }
        }

        tx.commit()?;
    }

    Ok(())
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