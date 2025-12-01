use std::time::Instant;

use alloy_primitives::{Address, B256, U256};
use rand::Rng;
use reth_chainspec::MAINNET;
use reth_primitives_traits::{Account, StorageEntry};
use reth_provider::{
    test_utils::create_test_provider_factory_with_chain_spec,
    DatabaseProviderFactory, HashingWriter, ProviderFactory, TrieWriter,
};
use reth_storage_api::TrieWriter as _;
use reth_db_common::init::compute_state_root;
use reth_db_common::init_triedb::calculate_state_root_with_triedb;

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
            let mut key_bytes = [0u8; 32];
            rng.fill(&mut key_bytes);
            let key = B256::from(key_bytes);

            let mut value_bytes = [0u8; 32];
            rng.fill(&mut value_bytes);
            let value = U256::from_be_slice(&value_bytes);

            storage_vec.push(StorageEntry { key, value });
        }
        storage_entries.push((address, storage_vec));
    }

    (accounts, storage_entries)
}

fn setup_test_data(
    num_accounts: usize,
    storage_per_account: usize,
) -> ProviderFactory<reth_provider::test_utils::MockNodeTypesWithDB> {
    let mut rng = rand::thread_rng();
    let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());

    let (accounts, storage_entries) =
        generate_random_accounts_and_storage(num_accounts, storage_per_account, &mut rng);

    // single RW tx to populate DB, then commit
    let mut provider_rw = provider_factory.provider_rw().unwrap();

    let accounts_for_hashing = accounts
        .iter()
        .map(|(address, account)| (*address, Some(*account)));

    provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();
    provider_rw.insert_storage_for_hashing(storage_entries).unwrap();
    provider_rw.commit().unwrap();

    provider_factory
}

fn main() {
    // args: traditional | triedb [num_accounts] [storage_per_account]
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "traditional".to_string());
    let num_accounts: usize = args.next().unwrap_or_else(|| "100000".to_string()).parse().unwrap();
    let storage_per_account: usize =
        args.next().unwrap_or_else(|| "5".to_string()).parse().unwrap();

    println!(
        "Running state root with mode={mode}, num_accounts={num_accounts}, storage_per_account={storage_per_account}"
    );

    let provider_factory = setup_test_data(num_accounts, storage_per_account);

    match mode.as_str() {
        "traditional" => {
            let provider_rw = provider_factory.provider_rw().unwrap();
            let start = Instant::now();
            let root = compute_state_root(&*provider_rw, None).unwrap();
            // If you want to persist trie tables, commit here:
            provider_rw.commit().unwrap();
            let elapsed = start.elapsed();
            println!("traditional: root={root:?}, elapsed={:?}", elapsed);
        }
        "triedb" => {
            use tempdir::TempDir;

            let provider_rw = provider_factory.provider_rw().unwrap();
            let tmp_dir = TempDir::new("state_root_triedb").unwrap();
            let trie_db_path = tmp_dir.path().join("triedb.db");

            let start = Instant::now();
            let root =
                calculate_state_root_with_triedb(&*provider_rw, trie_db_path.clone(), None).unwrap();
            let elapsed = start.elapsed();
            println!(
                "triedb: root={root:?}, elapsed={:?}",
                elapsed
            );
        }
        other => {
            eprintln!("Unknown mode: {other}. Use 'traditional' or 'triedb'.");
            std::process::exit(1);
        }
    }
}
