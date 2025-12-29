use alloy_primitives::{keccak256, Address, B256, U256, StorageKey, StorageValue};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_provider::{
    DatabaseProviderFactory, HashingWriter, LatestStateProvider, TrieWriter,
};
use reth_primitives_traits::Account;
use reth_storage_api::{StateRootProvider};
use reth_trie_common::{HashedPostState, HashedStorage};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::map::B256Map;
use tempdir::TempDir;
use triedb::{
    account::Account as TrieDBAccount,
    overlay::{OverlayStateMut, OverlayValue},
    path::{AddressPath, StoragePath},
    Database,
};
use reth_db::{init_db, ClientVersion, DatabaseEnv};
use reth_db::mdbx::DatabaseArguments;
use reth_db_common::init::compute_state_root;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpPrimitives;
use crate::util::{setup_tdb_database};

mod util;

fn main() -> eyre::Result<()> {
    println!("Testing overlay state root calculation methods...");

    // Generate shared test data
    let (base_addresses, base_accounts_map, base_storage_map, overlay_acct, overlay_storage) =
        util::generate_shared_test_data(
            util::DEFAULT_SETUP_DB_EOA_SIZE,
            util::DEFAULT_SETUP_DB_CONTRACT_SIZE,
            util::DEFAULT_SETUP_DB_STORAGE_PER_CONTRACT,
            util::BATCH_SIZE,
        );

    println!("Generated {} base addresses, {} overlay accounts, overlay storage {}", base_addresses.len(), overlay_acct.len(), overlay_storage.len());

    // Convert base_accounts_map and base_storage_map to genesis alloc format
    let mut genesis_alloc: BTreeMap<Address, GenesisAccount> = BTreeMap::new();

    for (address, account) in &base_accounts_map {
        // Convert storage from HashMap<B256, U256> to BTreeMap<B256, B256>
        let storage = base_storage_map.get(address).map(|storage_map| {
            storage_map
                .iter()
                .filter(|(_, v)| !v.is_zero()) // Only include non-zero storage values
                .map(|(k, v)| {
                    // Convert U256 to B256 for storage value
                    (*k, B256::from_slice(&v.to_be_bytes::<32>()))
                })
                .collect::<BTreeMap<B256, B256>>()
        });

        let genesis_account = GenesisAccount {
            nonce: Some(account.nonce),
            balance: account.balance,
            code: None, // We only have bytecode_hash, not the actual code
            storage: storage.filter(|s| !s.is_empty()),
            private_key: None,
        };
        
        genesis_alloc.insert(*address, genesis_account);
    }

    // Create Genesis struct with the alloc
    let genesis = Genesis {
        alloc: genesis_alloc,
        ..Genesis::default()
    };

    // Write to genesis.json file
    let genesis_json_path = PathBuf::from("genesis_random.json");
    let json_string = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&genesis_json_path, json_string)?;
    println!("Written genesis alloc to {}", genesis_json_path.display());

    let dir = TempDir::new("triedb_overlay_base").unwrap();
    let main_file_name_path = dir.path().join("triedb");
    let triedb = Database::create_new(&main_file_name_path).unwrap();

    setup_tdb_database(&triedb, &base_addresses, &base_accounts_map, &base_storage_map).unwrap();

    let mut account_overlay_mut = OverlayStateMut::new();

    for (address, account) in &overlay_acct {
        let address_path = AddressPath::for_address(*address);
        let trie_account = TrieDBAccount::new(
            account.nonce,
            account.balance,
            EMPTY_ROOT_HASH,
            account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
        );
        account_overlay_mut.insert(address_path.clone().into(), Some(OverlayValue::Account(trie_account)));
    }

    // Add overlay storage
    for (address, storage) in &overlay_storage {
        let address_path = AddressPath::for_address(*address);
        for (storage_key, storage_value) in storage {
            // Convert B256 back to U256 to get the raw storage slot
            let raw_slot = U256::from_be_slice(storage_key.as_slice());
            let storage_path = StoragePath::for_address_path_and_slot(
                address_path.clone(),
                StorageKey::from(raw_slot),
            );

            if storage_value.is_zero() {
                // Zero value means delete the storage slot
                account_overlay_mut.insert(
                    storage_path.clone().into(),
                    None,  // ✅ Delete slot for zero values
                );
            } else {
                // Non-zero value: insert the storage entry
                account_overlay_mut.insert(
                    storage_path.clone().into(),
                    Some(OverlayValue::Storage(StorageValue::from_be_slice(
                        storage_value.to_be_bytes::<32>().as_slice()
                    ))),
                );
            }
        }
    }
    let account_overlay = account_overlay_mut.freeze();

    let start = Instant::now();
    let tx = triedb.begin_ro()?;
    let triedb_root = tx.compute_root_with_overlay(account_overlay.clone())?;
    println!("triedb_root = {:?}, overlay state root elapsed = {:?} ms", triedb_root.root, start.elapsed().as_millis());

    let start = Instant::now();
    tx.commit()?;
    println!("triedb commit elapsed = {:?} ns", start.elapsed().as_nanos());

    // ===== Setup MDBX =====
    println!("\nSetting up MDBX...");
    // Create a chain spec with empty genesis allocation but keep base mainnet hardforks
    let empty_chain_spec = Arc::new(
        OpChainSpecBuilder::base_mainnet()
            .genesis(Genesis::default())  // Empty genesis with no alloc
            .build(),
    );


    let datadir = tempdir::TempDir::new("state_root_overlay")?;
    let db_path = datadir.path().join("mdbx");
    let sf_path = datadir.path().join("static_files");
    let triedb_path = datadir.path().join("triedb");
    reth_fs_util::create_dir_all(&db_path)?;
    reth_fs_util::create_dir_all(&sf_path)?;
    reth_fs_util::create_dir_all(&triedb_path)?;

    let db = Arc::new(init_db(
        &db_path,
        DatabaseArguments::new(ClientVersion::default()),
    )?);

    use reth_provider::providers::StaticFileProvider;
    let sfp: StaticFileProvider<OpPrimitives> = StaticFileProvider::read_write(sf_path)?;

    use reth_provider::providers::triedb::TriedbProvider;
    let triedb_provider = Arc::new(TriedbProvider::new(&triedb_path));

    use reth_provider::providers::ProviderFactory;
    let provider_factory: ProviderFactory<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>> = 
        ProviderFactory::new(
            db,
            empty_chain_spec.clone(),
            sfp,
            triedb_provider,
        );
    // Insert base data
    {
        let mut provider_rw = provider_factory.provider_rw()?;
        let accounts: Vec<(Address, Account)> = base_accounts_map.iter().map(|(a, acc)| (*a, *acc)).collect();
        let storage_entries: Vec<(Address, Vec<reth_primitives_traits::StorageEntry>)> = base_storage_map
            .iter()
            .map(|(address, storage)| {
                let entries: Vec<reth_primitives_traits::StorageEntry> = storage
                    .iter()
                    .map(|(key, value)| reth_primitives_traits::StorageEntry {
                        key: *key,
                        value: *value,
                    })
                    .collect();
                (*address, entries)
            })
            .collect();

        let accounts_for_hashing = accounts.iter().map(|(address, account)| (*address, Some(*account)));
        provider_rw.insert_account_for_hashing(accounts_for_hashing)?;
        provider_rw.insert_storage_for_hashing(storage_entries)?;

        let ret = compute_state_root(provider_rw.as_ref(), None)?;
        provider_rw.commit()?;

    }

    // Build HashedPostState from overlay
    let mut hashed_accounts: Vec<(B256, Option<Account>)> = overlay_acct
        .iter()
        .map(|(address, account)| {
            let hashed = keccak256(address);
            (hashed, Some(*account))
        })
        .collect();

    let mut hashed_storages: B256Map<HashedStorage> = HashMap::default();
    for (address, storage) in &overlay_storage {
        let hashed_address = keccak256(address);
        let hashed_storage = HashedStorage::from_iter(
            false,
            storage.iter().map(|(key, value)| {
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

    let db_provider_ro = provider_factory.database_provider_ro()?;
    let latest_ro = LatestStateProvider::new(db_provider_ro);

    let start = Instant::now();
    let (mdbx_root, _updates) = latest_ro.state_root_with_updates(hashed_state)?;

    println!("MDBX state root: {:?}, overlay state root elapsed {:?} ms", mdbx_root, start.elapsed().as_millis());
    assert_eq!(mdbx_root, triedb_root.root);

    Ok(())
}