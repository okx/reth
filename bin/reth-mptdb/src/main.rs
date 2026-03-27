//! Reth node with mptdb as the state backend.
//!
//! Architecture:
//! - **SC layer** (MptCommitStore): always-resident MPT, computes state root per block
//! - **PlainState** (reth MDBX): account/storage reads during EVM execution
//!
//! Integration points:
//! - `StateProviderOverride`: wraps reth's default provider so state_root / proof calls are served
//!   by SC while EVM reads (basic_account/storage) delegate to reth's PlainState via the
//!   `default_provider` passed to the override callback.
//! - `on_canonical_commit`: commits each canonical block's state changes to SC.
//!
//! Unlike reth-qmdb, state root validation is NOT skipped because mptdb uses
//! Keccak-256 paths (same as Ethereum), so roots are valid.
//!
//! Environment variables:
//! - `MPTDB_SC_PATH`: override SC data directory (default: datadir/mptdb/sc)

#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::map::HashMap;
use clap::{Args, Parser};
use mptdb_provider::{MptDbStateProvider, MptDbStateWriter};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
use parking_lot::Mutex;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::Receipt as EthReceipt;
use reth_node_builder::{DebugNodeLauncher, EngineNodeLauncher, NodeHandle};
use reth_node_ethereum::EthereumNode;
use reth_storage_api::StateWriter;
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;
use std::sync::Arc;
use tracing::info;

/// No extra CLI args for reth-mptdb (all config via env vars).
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
pub struct MptdbArgs {}

// ── DefaultProviderWrapper ────────────────────────────────────────────────────
// Wraps `StateProviderBox` (Box<dyn StateProvider + Send>) so it can be stored
// as `Arc<dyn StateProvider + Send + Sync>` in MptDbStateProvider.
//
// SAFETY: reth's DatabaseProvider (the source of default_provider in
// StateProviderOverride callbacks) is Send + Sync.  We re-assert Sync here to
// satisfy Arc's constraint.

use alloy_primitives::{Address, BlockNumber, Bytes, B256};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    errors::provider::ProviderResult, AccountReader, BlockHashReader, BytecodeReader,
    HashedPostStateProvider, StateProofProvider, StateProvider, StateProviderBox,
    StateRootProvider, StorageRootProvider,
};
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};

struct DefaultProviderWrapper(StateProviderBox);
unsafe impl Sync for DefaultProviderWrapper {}

// Only implement the traits actually required by StateProvider (its supertrait chain).
// BlockNumReader and BlockIdReader are NOT in StateProvider's supertrait chain.

impl AccountReader for DefaultProviderWrapper {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.0.basic_account(address)
    }
}
impl BlockHashReader for DefaultProviderWrapper {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }
    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}
impl BytecodeReader for DefaultProviderWrapper {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.0.bytecode_by_hash(code_hash)
    }
}
impl StateRootProvider for DefaultProviderWrapper {
    fn state_root(&self, h: HashedPostState) -> ProviderResult<B256> {
        self.0.state_root(h)
    }
    fn state_root_with_updates(&self, h: HashedPostState) -> ProviderResult<(B256, TrieUpdates)> {
        self.0.state_root_with_updates(h)
    }
    fn state_root_from_nodes(&self, i: TrieInput) -> ProviderResult<B256> {
        self.0.state_root_from_nodes(i)
    }
    fn state_root_from_nodes_with_updates(
        &self,
        i: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.0.state_root_from_nodes_with_updates(i)
    }
}
impl StorageRootProvider for DefaultProviderWrapper {
    fn storage_root(&self, a: Address, h: HashedStorage) -> ProviderResult<B256> {
        self.0.storage_root(a, h)
    }
    fn storage_proof(&self, a: Address, s: B256, h: HashedStorage) -> ProviderResult<StorageProof> {
        self.0.storage_proof(a, s, h)
    }
    fn storage_multiproof(
        &self,
        a: Address,
        slots: &[B256],
        h: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.0.storage_multiproof(a, slots, h)
    }
}
impl StateProofProvider for DefaultProviderWrapper {
    fn proof(&self, i: TrieInput, a: Address, slots: &[B256]) -> ProviderResult<AccountProof> {
        self.0.proof(i, a, slots)
    }
    fn multiproof(&self, i: TrieInput, t: MultiProofTargets) -> ProviderResult<MultiProof> {
        self.0.multiproof(i, t)
    }
    fn witness(&self, i: TrieInput, t: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        self.0.witness(i, t)
    }
}
impl HashedPostStateProvider for DefaultProviderWrapper {
    fn hashed_post_state(&self, b: &revm_database::BundleState) -> HashedPostState {
        self.0.hashed_post_state(b)
    }
}
impl StateProvider for DefaultProviderWrapper {
    fn storage(
        &self,
        account: Address,
        key: alloy_primitives::StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.0.storage(account, key)
    }
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = reth_ethereum_cli::Cli::<EthereumChainSpecParser, MptdbArgs>::parse().run(
        |builder, _args: MptdbArgs| async move {
            info!(target: "reth::cli", "Launching mptdb node");

            // ── mptdb paths ────────────────────────────────────────────────
            let data_dir = builder.config().datadir();
            let sc_path = std::env::var("MPTDB_SC_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| data_dir.data_dir().join("mptdb").join("sc"));

            std::fs::create_dir_all(&sc_path)?;

            info!(target: "reth::cli", sc = %sc_path.display(), "Opening mptdb");

            // ── Open SC ────────────────────────────────────────────────────
            let sc = Arc::new(Mutex::new(
                MptCommitStore::open(&sc_path, false)
                    .map_err(|e| eyre::eyre!("failed to open SC: {e}"))?,
            ));

            // ── Genesis pre-population ─────────────────────────────────────
            // Only populate if SC is at version 0 (fresh DB).
            if sc.lock().version() == 0 {
                let chain_spec = builder.config().chain.clone();
                let genesis = chain_spec.genesis();
                let mut state: HashMap<_, _> = HashMap::default();
                for (addr, account) in &genesis.alloc {
                    let info = AccountInfo {
                        nonce: account.nonce.unwrap_or_default(),
                        balance: account.balance,
                        code_hash: KECCAK_EMPTY,
                        code: None,
                        account_id: None,
                    };
                    let storage: revm_database::StorageWithOriginalValues = account
                        .storage
                        .as_ref()
                        .map(|s| {
                            s.iter()
                                .map(|(k, v)| {
                                    (
                                        (*k).into(),
                                        StorageSlot::new_changed(Default::default(), (*v).into()),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
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

                let writer = MptDbStateWriter::<EthReceipt>::new(sc.clone());
                writer
                    .pre_populate(&genesis_bundle, 0)
                    .map_err(|e| eyre::eyre!("genesis pre_populate failed: {e}"))?;
                info!(
                    target: "reth::cli",
                    accounts = genesis.alloc.len(),
                    "Pre-populated mptdb with genesis state"
                );
            } else {
                info!(
                    target: "reth::cli",
                    version = sc.lock().version(),
                    "mptdb already initialized, skipping genesis pre-populate"
                );
            }

            // ── on_canonical_commit: commit each canonical block's state to SC ─
            let sc_for_commit = sc.clone();

            let on_canonical_commit = Box::new(move |bundle: &revm_database::BundleState| {
                let writer = MptDbStateWriter::<EthReceipt>::new(sc_for_commit.clone());
                let outcome = reth_execution_types::ExecutionOutcome::<EthReceipt>::new(
                    bundle.clone(),
                    Default::default(),
                    0, // first_block placeholder (unused by SC writer)
                    Default::default(),
                );
                if let Err(e) = writer.write_state(
                    &outcome,
                    revm_database::OriginalValuesKnown::Yes,
                    reth_storage_api::StateWriteConfig::default(),
                ) {
                    tracing::error!(
                        target: "reth::cli",
                        error = %e,
                        "mptdb on_canonical_commit failed"
                    );
                }
            });

            // ── StateProviderOverride: SC provides state_root/proof; EVM reads
            //   delegate to reth's default_provider (PlainState via MDBX). ──
            let sc_for_override = sc.clone();
            let noop_block_id: Arc<dyn reth_storage_api::BlockIdReader + Send + Sync> =
                Arc::new(reth_storage_api::noop::NoopProvider::default());

            let state_override: reth_provider::providers::StateProviderOverride =
                Arc::new(move |default_provider| {
                    let version = sc_for_override.lock().version().max(0);
                    // Wrap Box<dyn StateProvider + Send> in DefaultProviderWrapper
                    // to satisfy Arc<dyn StateProvider + Send + Sync>.
                    let fallback: Arc<dyn StateProvider + Send + Sync> =
                        Arc::new(DefaultProviderWrapper(default_provider));
                    Box::new(MptDbStateProvider::new(
                        Arc::clone(&sc_for_override),
                        version,
                        fallback,
                        noop_block_id.clone(),
                    ))
                });

            // ── Engine configuration ───────────────────────────────────────
            // mptdb uses Keccak-256 paths → state root validation CAN be enabled.
            // Set skip_state_root_validation=false (default) for correctness.
            let engine_tree_config =
                builder.config().engine.tree_config().with_skip_state_root_validation(false);

            let task_executor = builder.task_executor().clone();
            let data_dir = builder.config().datadir();

            let launcher = EngineNodeLauncher::new(task_executor, data_dir, engine_tree_config)
                .with_on_canonical_commit(on_canonical_commit)
                .with_state_provider_override(state_override);

            // ── Launch ─────────────────────────────────────────────────────
            let NodeHandle { node: _node, node_exit_future } = builder
                .with_types::<EthereumNode>()
                .with_components(EthereumNode::components())
                .with_add_ons(reth_node_ethereum::node::EthereumAddOns::default())
                .launch_with(DebugNodeLauncher::new(launcher))
                .await?;

            info!(target: "reth::cli", "mptdb node launched");
            node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
