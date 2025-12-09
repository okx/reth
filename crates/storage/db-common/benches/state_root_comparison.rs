#![allow(missing_docs, unreachable_pub)]

mod util;

use alloy_primitives::{keccak256, Address, StorageKey, StorageValue, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::prelude::*;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use reth_chainspec::MAINNET;
use reth_primitives_traits::{Account, StorageEntry};
use reth_provider::LatestStateProvider;
use reth_provider::{
    test_utils::create_test_provider_factory_with_chain_spec,
    DatabaseProviderFactory, DBProvider, HashingWriter, ProviderFactory, TrieWriter,
};
use reth_storage_api::{StateRootProvider, TrieWriter as _};
use reth_trie::{HashedPostState, HashedStorage, StateRoot as StateRootComputer};
use reth_trie_db::DatabaseHashedCursorFactory;
use reth_trie::{StateRootTrieDb, TrieExtDatabase};
use std::path::PathBuf;
use std::time::Duration;
use alloy_primitives::map::{B256Map, HashMap};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use tempdir::TempDir;
use triedb::overlay::{OverlayStateMut, OverlayValue};
use triedb::{path::AddressPath, account::Account as TrieDBAccount, Database};
use triedb::path::StoragePath;
use reth_db_common::init::compute_state_root;
use reth_db_common::init_triedb::calculate_state_root_with_triedb;
use crate::util::{get_flat_trie_database, copy_files, DEFAULT_SETUP_DB_CONTRACT_SIZE, DEFAULT_SETUP_DB_EOA_SIZE, DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT, SEED_CONTRACT, BATCH_SIZE, generate_random_address};

fn generate_random_accounts_and_storage(
    num_accounts: usize,
    storage_per_account: usize,
    rng: &mut impl Rng,
) -> (Vec<(Address, Account)>, Vec<(Address, Vec<StorageEntry>)>) {
    let mut accounts = Vec::new();
    let mut storage_entries = Vec::new();

    for _ in 0..num_accounts {
        let mut address_bytes = [0u8; 20];
        rng.fill(&mut address_bytes);
        let address = Address::from_slice(&address_bytes);
        
        let account = Account {
            nonce: rng.gen_range(0..=u64::MAX),
            balance: U256::from(rng.gen_range(0u128..=u128::MAX)),
            bytecode_hash: {
                let mut hash_bytes = [0u8; 32];
                rng.fill(&mut hash_bytes);
                Some(B256::from(hash_bytes))
            },
        };
        accounts.push((address, account));

        let mut storage_vec = Vec::new();
        for _ in 0..storage_per_account {
            let mut storage_key_bytes = [0u8; 32];
            rng.fill(&mut storage_key_bytes);
            let storage_key = B256::from(storage_key_bytes);
            
            let mut storage_value_bytes = [0u8; 32];
            rng.fill(&mut storage_value_bytes);
            let storage_value = U256::from_be_slice(&storage_value_bytes);
            
            storage_vec.push(StorageEntry {
                key: storage_key,
                value: storage_value,
            });
        }
        storage_entries.push((address, storage_vec));
    }

    (accounts, storage_entries)
}

fn setup_test_data(
    num_accounts: usize,
    storage_per_account: usize,
) -> reth_provider::providers::ProviderFactory<reth_provider::test_utils::MockNodeTypesWithDB> {
    let mut rng = rand::thread_rng();
    let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());

    let (accounts, storage_entries) =
        generate_random_accounts_and_storage(num_accounts, storage_per_account, &mut rng);

    let mut provider_rw = provider_factory.provider_rw().unwrap();

    let accounts_for_hashing = accounts
        .iter()
        .map(|(address, account)| (*address, Some(*account)));

    provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();
    provider_rw.insert_storage_for_hashing(storage_entries).unwrap();
    provider_rw.commit().unwrap();

    provider_factory
}

pub fn bench_state_root_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("State Root Calculation");
    group.sample_size(10);

    for size in [100000] {
        let provider_factory = setup_test_data(size, 5);
        
        // Benchmark traditional method
        group.bench_function(BenchmarkId::new("traditional", size), |b| {
            b.iter(|| {
                let provider_rw = provider_factory.provider_rw().unwrap();
                compute_state_root(&*provider_rw, None).unwrap();
                provider_rw.commit().unwrap();
            })
        });

        // Benchmark TrieDB method
        group.bench_function(BenchmarkId::new("triedb", size), |b| {
            b.iter_with_setup(
                || {
                    let tmp_dir = TempDir::new("bench_triedb").unwrap();
                    let db_path = tmp_dir.path().join(format!("test_{}.db", size));
                    (tmp_dir, db_path)
                },
                |(tmp_dir, trie_db_path)| {
                    let provider = provider_factory.provider_rw().unwrap();
                    calculate_state_root_with_triedb(&*provider, trie_db_path, None).unwrap()
                },
            )
        });
    }

    group.finish();
}
fn bench_state_root_with_overlay_triedb(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_root_with_overlay");
    let (base_dir, (overlay_acct, overlay_storage)) = get_flat_trie_database(
        DEFAULT_SETUP_DB_EOA_SIZE,
        DEFAULT_SETUP_DB_CONTRACT_SIZE,
        DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT,
        BATCH_SIZE
    );
    let dir = TempDir::new("triedb_bench_state_root_with_overlay").unwrap();
    let file_name = base_dir.main_file_name.clone();
    copy_files(&base_dir, dir.path()).unwrap();

    // Generate overlay from the returned overlay data (accounts + storage)
    let mut account_overlay_mut = OverlayStateMut::new();

    // Add account overlays
    for (address, account) in &overlay_acct {
        let address_path = AddressPath::for_address(*address);
        let trie_account = TrieDBAccount::new(
            account.nonce,
            account.balance,
            EMPTY_ROOT_HASH,
            KECCAK_EMPTY,
        );
        account_overlay_mut.insert(address_path.clone().into(), Some(OverlayValue::Account(trie_account)));
    }

    // Add storage overlays
    for (address, storage) in &overlay_storage {
        let address_path = AddressPath::for_address(*address);
        for (storage_key, storage_value) in storage {
            let storage_path = StoragePath::for_address_path_and_slot(
                address_path.clone(),
                StorageKey::from(*storage_key),
            );
            account_overlay_mut.insert(
                storage_path.clone().into(),
                Some(OverlayValue::Storage(StorageValue::from_be_slice(
                    storage_path.get_slot().pack().as_slice()
                ))),
            );
        }
    }

    let account_overlay = account_overlay_mut.freeze();

    let overlay_count = overlay_acct.len() + overlay_storage.values().map(|s| s.len()).sum::<usize>();

    group.throughput(criterion::Throughput::Elements(overlay_count as u64));
    group.measurement_time(Duration::from_secs(30));
    group.bench_function(BenchmarkId::new("state_root_with_overlay_triedb", overlay_count), |b| {
        b.iter_with_setup(
            || {
                let db_path = dir.path().join(&file_name);
                Database::open(db_path).unwrap()
            },
            |db| {
                let tx = db.begin_ro().unwrap();

                let _root_result = tx.compute_root_with_overlay(account_overlay.clone()).unwrap();

                tx.commit().unwrap();
            },
        );
    });

    group.finish();
}

fn bench_state_root_with_overlay_mdbx(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_root_mdbx_with_overlay");

    // Generate random data and overlay
    let (addresses, accounts_map, storage_map, overlay_acct, overlay_storage) =
        util::generate_shared_test_data(
            DEFAULT_SETUP_DB_EOA_SIZE, // eoa_count
            DEFAULT_SETUP_DB_CONTRACT_SIZE, // contract_count
            DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT,
            BATCH_SIZE, // overlay_count
        );

    // Write base data into database using provider_rw
    let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());
    {
        let mut provider_rw = provider_factory.provider_rw().unwrap();

        // Convert base accounts to vector format
        let accounts: Vec<(Address, Account)> = accounts_map.into_iter().collect();
        let storage_entries: Vec<(Address, Vec<StorageEntry>)> = storage_map.into_iter()
            .map(|(address, storage)| {
                let entries: Vec<StorageEntry> = storage.into_iter()
                    .map(|(key, value)| StorageEntry { key, value })
                    .collect();
                (address, entries)
            })
            .collect();

        let accounts_for_hashing = accounts.iter().map(|(address, account)| (*address, Some(*account)));
        provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();
        provider_rw.insert_storage_for_hashing(storage_entries).unwrap();
        provider_rw.commit().unwrap();
    }

    // Create HashedPostState from overlay data
    let mut hashed_accounts: Vec<(B256, Option<Account>)> = overlay_acct.iter()
        .map(|(address, account)| {
            let hashed = keccak256(address);
            (hashed, Some(*account))
        })
        .collect();

    // Build HashedStorage for overlay storage
    let mut hashed_storages: B256Map<HashedStorage> = HashMap::default();
    for (address, storage) in &overlay_storage {
        let hashed_address = keccak256(address);
        let hashed_storage = HashedStorage::from_iter(
            false, // wiped = false
            storage.iter().map(|(key, value)| {
                // key is a raw storage slot (B256), need to hash it
                let hashed_slot = keccak256(*key);
                (hashed_slot, *value)
            }),
        );
        hashed_storages.insert(hashed_address, hashed_storage);
    }

    let hashed_state = HashedPostState {
        accounts: hashed_accounts.into_iter().collect(),
        storages: hashed_storages,
    };

    // Use provider_ro for state_root_with_updates
    let db_provider_ro = provider_factory.database_provider_ro().unwrap();
    let latest_ro = LatestStateProvider::new(db_provider_ro);

    let overlay_count = overlay_acct.len() + overlay_storage.values().map(|s| s.len()).sum::<usize>();

    group.throughput(criterion::Throughput::Elements(overlay_count as u64));
    group.measurement_time(Duration::from_secs(30));
    group.bench_function(BenchmarkId::new("state_root_with_overlay_mdbx", overlay_count), |b| {
        b.iter(|| {
            let _ = latest_ro.state_root_with_updates(hashed_state.clone());
        })
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_state_root_comparison, bench_state_root_with_overlay_triedb, bench_state_root_with_overlay_mdbx
}
criterion_main!(benches);
