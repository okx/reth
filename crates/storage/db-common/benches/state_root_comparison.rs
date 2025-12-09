#![allow(missing_docs, unreachable_pub)]

mod util;

use alloy_primitives::{keccak256,Address, B256, U256};
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
use reth_trie::{HashedPostState, StateRoot as StateRootComputer};
use reth_trie_db::DatabaseHashedCursorFactory;
use reth_trie::{StateRootTrieDb, TrieExtDatabase};
use std::path::PathBuf;
use std::time::Duration;
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use tempdir::TempDir;
use triedb::overlay::{OverlayStateMut, OverlayValue};
use triedb::{path::AddressPath, account::Account as TrieDBAccount, Database};
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

fn bench_state_root_with_overlay(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_root_with_overlay");
    let base_dir = get_flat_trie_database(
        DEFAULT_SETUP_DB_EOA_SIZE,
        DEFAULT_SETUP_DB_CONTRACT_SIZE,
        DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT,
    );
    let dir = TempDir::new("triedb_bench_state_root_with_overlay").unwrap();
    let file_name = base_dir.main_file_name.clone();
    copy_files(&base_dir, dir.path()).unwrap();

    let mut rng = StdRng::seed_from_u64(SEED_CONTRACT);
    let addresses: Vec<AddressPath> =
        (0..BATCH_SIZE).map(|_| generate_random_address(&mut rng)).collect();

    let mut account_overlay_mut = OverlayStateMut::new();
    addresses.iter().enumerate().for_each(|(i, addr)| {
        let new_account =
            TrieDBAccount::new(i as u64, U256::from(i as u64), EMPTY_ROOT_HASH, KECCAK_EMPTY);
        account_overlay_mut.insert(addr.clone().into(), Some(OverlayValue::Account(new_account)));
    });
    let account_overlay = account_overlay_mut.freeze();

    group.throughput(criterion::Throughput::Elements(BATCH_SIZE as u64));
    group.measurement_time(Duration::from_secs(30));
    group.bench_function(BenchmarkId::new("state_root_with_account_overlay", BATCH_SIZE), |b| {
        b.iter_with_setup(
            || {
                let db_path = dir.path().join(&file_name);
                Database::open(db_path).unwrap()
            },
            |db| {
                let tx = db.begin_ro().unwrap();

                // Compute the root hash with the overlay
                let _root_result = tx.compute_root_with_overlay(account_overlay.clone()).unwrap();

                tx.commit().unwrap();
            },
        );
    });

    group.finish();

}

fn bench_state_root_mdbx_with_overlay(c: &mut Criterion) {
    let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());
    let db_provider_ro = provider_factory.database_provider_ro().unwrap();
    let latest_ro = LatestStateProvider::new(db_provider_ro);
    let mut rng = rand::thread_rng();
    let hashed_accounts: Vec<(B256, Option<Account>)> = (0..1000).map(|_| {
        let mut addr = [0u8; 20];
        rng.fill(&mut addr);
        let hashed = keccak256(addr);
        let acct = Account {
            nonce: rng.gen_range(0..=u64::MAX),
            balance: U256::from(rng.gen_range(0u128..=u128::MAX)),
            bytecode_hash: {
                let mut hash_bytes = [0u8; 32];
                rng.fill(&mut hash_bytes);
                Some(B256::from(hash_bytes))
            },
        };
        (hashed, Some(acct))
    }).collect();
    let hashed_state = HashedPostState::default().with_accounts(hashed_accounts);
    c.bench_with_input(
        BenchmarkId::new("bench_state_root_mdbx_with_overlay", 1000),
        &hashed_state,
        |b, hs| {
            b.iter(|| {
                let _ = latest_ro.state_root_with_updates(hs.clone());
            })
        },
    );
}


criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_state_root_comparison, bench_state_root_with_overlay, bench_state_root_mdbx_with_overlay
}
criterion_main!(benches);
