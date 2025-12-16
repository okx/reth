use alloy_primitives::{address, keccak256, B256, U256};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_primitives_traits::Account;
use reth_provider::{
    test_utils::create_test_provider_factory_with_chain_spec,DatabaseProviderFactory,
    LatestStateProvider, ProviderFactory,
};
use reth_storage_api::StateRootProvider;
use reth_trie_common::HashedPostState;
use std::sync::Arc;
use alloy_genesis::Genesis;
use tempdir::TempDir;
use triedb::{
    account::Account as TrieDBAccount,
    overlay::{OverlayStateMut, OverlayValue},
    path::AddressPath,
    Database,
};

fn main() -> eyre::Result<()> {
    println!("Testing overlay state root calculation with single account...");

    // ===== Setup TrieDB =====
    let dir = TempDir::new("triedb_overlay_min").unwrap();
    let main_file_name_path = dir.path().join("triedb");
    let triedb = Database::create_new(&main_file_name_path).unwrap();

    let tdb_pre_root = triedb.state_root();
    println!("TrieDB pre state root: {:?}", tdb_pre_root);

    // Create overlay with single account
    let mut overlay_mut = OverlayStateMut::new();
    let address = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
    let address_path = AddressPath::for_address(address);
    let trie_account = TrieDBAccount::new(
        1,                      // nonce
        U256::from(100),       // balance
        EMPTY_ROOT_HASH,       // storage_root
        KECCAK_EMPTY,          // code_hash
    );
    overlay_mut.insert(address_path.clone().into(), Some(OverlayValue::Account(trie_account)));
    let account_overlay = overlay_mut.freeze();

    // Calculate state root with TrieDB
    let tx = triedb.begin_ro()?;
    let triedb_root = tx.compute_root_with_overlay(account_overlay.clone())?;
    println!("TrieDB state root with overlay: {:?}", triedb_root.root);
    tx.commit()?;

    // ===== Setup MDBX =====
    println!("\nSetting up MDBX...");
    let empty_chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(Genesis::default())
            .with_forks(MAINNET.hardforks.clone())
            .build(),
    );
    let provider_factory = create_test_provider_factory_with_chain_spec(empty_chain_spec);

    let db_provider_ro_pre = provider_factory.database_provider_ro()?;
    let latest_ro_pre = LatestStateProvider::new(db_provider_ro_pre);
    let empty_state = HashedPostState::default();
    let (mdbx_pre_root, _) = latest_ro_pre.state_root_with_updates(empty_state)?;
    println!("MDBX pre state root: {:?}", mdbx_pre_root);

    // Build HashedPostState from overlay (single account)
    let account = Account {
        nonce: 1,
        balance: U256::from(100),
        bytecode_hash: None, // No bytecode
    };
    let hashed_address = keccak256(address);
    let hashed_state = HashedPostState {
        accounts: vec![(hashed_address, Some(account))].into_iter().collect(),
        storages: Default::default(),
    };

    // Calculate state root with MDBX
    let db_provider_ro = provider_factory.database_provider_ro()?;
    let latest_ro = LatestStateProvider::new(db_provider_ro);
    let (mdbx_root, _updates) = latest_ro.state_root_with_updates(hashed_state)?;
    println!("MDBX state root with overlay: {:?}", mdbx_root);

    // ===== Compare Results =====
    println!("\n=== Comparison ===");
    println!("TrieDB root: {:?}", triedb_root.root);
    println!("MDBX root:   {:?}", mdbx_root);

    if triedb_root.root == mdbx_root {
        println!("\n✅ SUCCESS: Both methods produce the same state root!");
        Ok(())
    } else {
        println!("\n❌ FAILURE: State roots differ!");
        eyre::bail!("State root mismatch: TrieDB={:?}, MDBX={:?}", triedb_root.root, mdbx_root)
    }
}