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
use mptdb_provider::{MptDbStateProvider, MptDbStateWriter, SyncProvider};
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

// StateProviderOverride wraps default_provider in SyncProvider (Mutex-based)
// to satisfy Arc<dyn StateProvider + Send + Sync>.
use reth_storage_api::StateProvider;

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
            //
            // Known limitation — MDBX-exists-but-SC-lost scenario:
            // This check only guards against re-populating an existing SC.
            // If the MDBX datadir already contains a non-genesis chain state
            // (e.g. a synced node) but the SC directory was deleted or
            // re-created, SC will be seeded from genesis and diverge from the
            // actual chain state.  The node will produce wrong state roots
            // from block 1 onwards without any error.
            // Mitigation: if MDBX has non-genesis state, SC should be rebuilt
            // by replaying canonical blocks before starting the node.
            // TODO: detect this condition by comparing best_block_number() from
            // reth's provider against sc.version() == 0 and fail-fast if
            // MDBX is ahead.
            if sc.lock().version() == 0 {
                let chain_spec = builder.config().chain.clone();
                let genesis = chain_spec.genesis();
                let mut state: HashMap<_, _> = HashMap::default();
                let mut contracts: HashMap<alloy_primitives::B256, revm_state::Bytecode> =
                    HashMap::default();
                for (addr, account) in &genesis.alloc {
                    // Compute real code_hash for accounts that have code.
                    // Using KECCAK_EMPTY for all accounts (including contracts) would
                    // produce wrong state roots for chains with genesis contracts
                    // (commit_store uses code_hash in account RLP encoding).
                    let (code_hash, code) = match &account.code {
                        Some(code_bytes) => {
                            let hash = alloy_primitives::keccak256(code_bytes);
                            let bytecode =
                                revm_state::Bytecode::new_raw(code_bytes.clone().0.into());
                            contracts.insert(hash, bytecode);
                            (hash, None)
                        }
                        None => (KECCAK_EMPTY, None),
                    };
                    let info = AccountInfo {
                        nonce: account.nonce.unwrap_or_default(),
                        balance: account.balance,
                        code_hash,
                        code,
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
                    contracts,
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
            //
            // Reorg handling:
            // The callback now receives (block_number, block_hash, bundle).
            // If a new canonical block number is <= last committed SC block,
            // we rollback SC to (block_number - 1) before applying the new
            // block state. This keeps SC aligned with canonical chain on reorgs.
            //
            // Performance note — SC commit is synchronous and blocks the engine:
            // The callback is invoked inside `on_canonical_chain_update` which runs
            // on the engine's main task (tree/mod.rs:2371).  SC apply+WAL+root runs
            // synchronously here, adding its latency directly to canonical chain
            // processing.  reth-qmdb uses an async flush model (reth-qmdb/main.rs)
            // to avoid this.  mptdb's wal_first_commit mode reduces the critical-path
            // cost (WAL write is fast; segment build is deferred to a background
            // worker), but the apply+root phase still blocks inline.
            let sc_for_commit = sc.clone();
            // Tracks latest block committed into SC; used to detect reorgs and
            // execute rollback before applying replacement canonical blocks.
            let last_sc_committed_block = Arc::new(Mutex::new({
                let v = sc_for_commit.lock().version();
                if v > 0 { Some((v - 1) as u64) } else { None }
            }));
            // Tracks latest canonical block hash that SC is aligned with.
            // StateProviderOverride uses this to gate SC usage to latest-only.
            let latest_sc_hash = Arc::new(Mutex::new(None::<alloy_primitives::B256>));
            let latest_sc_hash_for_commit = latest_sc_hash.clone();
            let last_sc_committed_block_for_commit = last_sc_committed_block.clone();

            let on_canonical_commit = Box::new(
                move |
                      block_number: alloy_primitives::BlockNumber,
                      block_hash: alloy_primitives::BlockHash,
                      bundle: &revm_database::BundleState| {
                let writer = MptDbStateWriter::<EthReceipt>::new(sc_for_commit.clone());

                // Reorg handling: if new canonical block number is <= last committed
                // SC block, rollback SC to (new_first - 1) before applying.
                if let Some(last_committed) = *last_sc_committed_block_for_commit.lock() {
                    if block_number <= last_committed {
                        let rollback_to = block_number.saturating_sub(1);
                        writer.remove_state_above(rollback_to).unwrap_or_else(|e| {
                            panic!(
                                "mptdb: SC rollback failed at reorg (target block {rollback_to}): {e}"
                            )
                        });
                    } else {
                        let expected_next = last_committed.saturating_add(1);
                        if block_number != expected_next {
                            panic!(
                                "mptdb: non-contiguous canonical callback: expected block {expected_next}, got {block_number}"
                            );
                        }
                    }
                }

                let outcome = reth_execution_types::ExecutionOutcome::<EthReceipt>::new(
                    bundle.clone(),
                    Default::default(),
                    0, // first_block placeholder (unused by SC writer)
                    Default::default(),
                );
                // SC commit failure is unrecoverable: all subsequent
                // state_root / proof calls will be based on a diverged SC
                // state, silently producing wrong roots.  Panic to abort the
                // node rather than allowing it to continue with corrupted state.
                writer
                    .write_state(
                        &outcome,
                        revm_database::OriginalValuesKnown::Yes,
                        reth_storage_api::StateWriteConfig::default(),
                    )
                    .unwrap_or_else(|e| {
                        panic!(
                            "mptdb: SC commit failed — aborting to prevent state divergence: {e}"
                        )
                    });
                *last_sc_committed_block_for_commit.lock() = Some(block_number);
                *latest_sc_hash_for_commit.lock() = Some(block_hash);
            });

            // ── StateProviderOverride: SC provides state_root/proof; EVM reads
            //   delegate to reth's default_provider (PlainState via MDBX). ──
            let sc_for_override = sc.clone();
            let latest_sc_hash_for_override = latest_sc_hash.clone();
            // NoopProvider is used here because the StateProviderOverride is only
            // invoked via BlockchainProvider::state_by_block_hash (engine execution
            // path), which does not call block_id_reader methods (chain_info,
            // block_number, etc.) on MptDbStateProvider.  If this provider is used
            // in a context where those methods ARE called (e.g. direct RPC via
            // MptDbStateProviderFactory), the NoopProvider will return empty/zero
            // values.  Fix: wire in BlockchainProvider or a real block reader.
            let noop_block_id: Arc<dyn reth_storage_api::BlockIdReader + Send + Sync> =
                Arc::new(reth_storage_api::noop::NoopProvider::default());

            let state_override: reth_provider::providers::StateProviderOverride =
                Arc::new(move |requested_hash, default_provider| {
                    // Historical hash requests must not use SC (SC has no
                    // per-version snapshots yet).  Delegate them to reth's
                    // default provider for correctness.
                    let use_sc = latest_sc_hash_for_override
                        .lock()
                        .as_ref()
                        .is_some_and(|h| *h == requested_hash);
                    if !use_sc {
                        return default_provider
                    }
                    // SC version = latest committed block + 1.
                    // This callback is invoked via BlockchainProvider::state_by_block_hash()
                    // for the block currently being executed.  For the normal execution path
                    // (EVM executing block N+1, parent = block N), SC is at version N which
                    // is the correct base for state_root / proof.
                    //
                    // Historical hash queries are delegated to default_provider.
                    // SC is used only when requested_hash == latest_sc_hash.
                    // This avoids serving incorrect historical roots/proofs from
                    // SC, which currently has no per-version snapshots.
                    let version = sc_for_override.lock().version().max(0);
                    // SyncProvider wraps StateProviderBox in Mutex<> to satisfy
                    // Arc<dyn StateProvider + Send + Sync> without unsafe.
                    // EVM reads are single-threaded per block so the Mutex is
                    // uncontended in the engine execution path.
                    let fallback: Arc<dyn StateProvider + Send + Sync> =
                        SyncProvider::new(default_provider);
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
