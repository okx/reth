#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::map::HashMap;
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_builder::{DebugNodeLauncher, EngineNodeLauncher, NodeHandle};
use reth_node_ethereum::EthereumNode;
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;
use std::sync::Arc;
use tracing::info;
use xlayer_qmdb_provider::QmdbStore;

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = reth_ethereum_cli::interface::Cli::<EthereumChainSpecParser>::parse()
        .run(|builder, _| async move {
            // QMDB path: use QMDB_PATH env var if set, otherwise <datadir>/qmdb
            let qmdb_path = std::env::var("QMDB_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| builder.config().datadir().data_dir().join("qmdb"));

            // Clean QMDB directory on startup for fresh benchmark runs.
            if qmdb_path.exists() {
                info!(target: "reth::cli", path = %qmdb_path.display(), "Removing existing QMDB directory");
                std::fs::remove_dir_all(&qmdb_path)?;
            }

            info!(target: "reth::cli", path = %qmdb_path.display(), "Initializing QMDB store");
            let qmdb_store = Arc::new(QmdbStore::new(&qmdb_path));

            // Pre-populate QMDB with genesis state from chain spec.
            let chain_spec = builder.config().chain.clone();
            let genesis = chain_spec.genesis();
            let mut state = HashMap::default();
            for (addr, account) in &genesis.alloc {
                let info = AccountInfo {
                    nonce: account.nonce.unwrap_or_default(),
                    balance: account.balance,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                };
                let mut storage = Default::default();
                if let Some(ref account_storage) = account.storage {
                    storage = account_storage
                        .iter()
                        .map(|(k, v)| {
                            ((*k).into(), StorageSlot::new_changed(Default::default(), (*v).into()))
                        })
                        .collect();
                }
                state.insert(
                    *addr,
                    BundleAccount {
                        info: Some(info),
                        original_info: None,
                        storage,
                        status: AccountStatus::Changed,
                    },
                );
            }
            let genesis_bundle = BundleState {
                state,
                contracts: Default::default(),
                reverts: Default::default(),
                state_size: 0,
                reverts_size: 0,
            };
            let num_chunks = qmdb_store.pre_populate(&genesis_bundle);
            info!(target: "reth::cli", accounts = genesis.alloc.len(), chunks = num_chunks, "Pre-populated QMDB with genesis state");

            // Callback: commit each new canonical block's BundleState to QMDB.
            let store_for_commit = qmdb_store.clone();
            let on_canonical_commit = Box::new(move |bundle: &revm_database::BundleState| {
                store_for_commit.commit_bundle(bundle);
            });

            // State provider override: all state reads go through QMDB.
            let store_for_override = qmdb_store.clone();
            let state_override: reth_provider::providers::StateProviderOverride =
                Arc::new(move || {
                    Box::new(xlayer_qmdb_provider::QmdbStateProvider::new(
                        store_for_override.clone(),
                    ))
                });

            // Configure engine tree: skip state root validation.
            let engine_tree_config = builder
                .config()
                .engine
                .tree_config()
                .with_skip_state_root_validation(true);

            let task_executor = builder.task_executor().clone();
            let data_dir = builder.config().datadir();

            let launcher = EngineNodeLauncher::new(task_executor, data_dir, engine_tree_config)
                .with_on_canonical_commit(on_canonical_commit)
                .with_state_provider_override(state_override);

            let NodeHandle { node: _node, node_exit_future } = builder
                .node(EthereumNode::default())
                .launch_with(DebugNodeLauncher::new(launcher))
                .await?;

            info!(target: "reth::cli", "QMDB node launched successfully");

            node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
