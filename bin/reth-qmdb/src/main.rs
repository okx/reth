#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

mod parallel_payload;

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::map::HashMap;
use clap::{Args, Parser};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_engine_primitives::{
    EthBuiltPayload, EthPayloadAttributes, EthPayloadBuilderAttributes,
};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeTypes, NodeTypes, PrimitivesTy, TxTy};
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, PayloadBuilderBuilder},
    BuilderContext, DebugNodeLauncher, EngineNodeLauncher, Node, NodeHandle, PayloadBuilderConfig,
    PayloadTypes,
};
use reth_node_ethereum::EthereumNode;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;
use std::sync::Arc;
use tracing::info;
use xlayer_qmdb_provider::QmdbStore;

/// XLayer-specific CLI arguments for reth-qmdb
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "XLayer")]
pub struct QmdbArgs {
    /// Enable parallel transaction execution (background simulation for CrwSets)
    #[arg(
        long = "xlayer.parallel-exec",
        help = "Enable parallel transaction execution for mempool transactions (disabled by default)",
        default_value = "false"
    )]
    pub parallel_exec: bool,
}

/// Custom payload builder that passes parallel_exec flag to EthereumBuilderConfig
#[derive(Clone, Debug)]
struct QmdbPayloadBuilder {
    parallel_exec: bool,
    /// QMDB store for the parallel payload builder to commit state and compute correct root.
    qmdb_store: Option<Arc<QmdbStore>>,
}

/// Enum to hold either the sequential or parallel payload builder.
#[derive(Debug, Clone)]
enum QmdbPayloadBuilderInner<Pool, Provider, Evm> {
    Sequential(reth_ethereum_payload_builder::EthereumPayloadBuilder<Pool, Provider, Evm>),
    Parallel(parallel_payload::ParallelPayloadBuilder<Pool, Provider, Evm>),
}

impl<Pool, Provider, Evm> reth_basic_payload_builder::PayloadBuilder
    for QmdbPayloadBuilderInner<Pool, Provider, Evm>
where
    Evm: ConfigureEvm<
        Primitives = EthPrimitives,
        NextBlockEnvCtx = reth_evm::NextBlockEnvAttributes,
    >,
    Provider: reth_storage_api::StateProviderFactory
        + reth_chainspec::ChainSpecProvider<ChainSpec: EthereumHardforks>
        + Clone,
    Pool: TransactionPool<
        Transaction: PoolTransaction<Consensus = reth_ethereum_primitives::TransactionSigned>,
    >,
{
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: reth_basic_payload_builder::BuildArguments<
            EthPayloadBuilderAttributes,
            EthBuiltPayload,
        >,
    ) -> Result<
        reth_basic_payload_builder::BuildOutcome<EthBuiltPayload>,
        reth_payload_builder_primitives::PayloadBuilderError,
    > {
        match self {
            Self::Sequential(b) => b.try_build(args),
            Self::Parallel(b) => b.try_build(args),
        }
    }

    fn on_missing_payload(
        &self,
        args: reth_basic_payload_builder::BuildArguments<
            EthPayloadBuilderAttributes,
            EthBuiltPayload,
        >,
    ) -> reth_basic_payload_builder::MissingPayloadBehaviour<EthBuiltPayload> {
        match self {
            Self::Sequential(b) => b.on_missing_payload(args),
            Self::Parallel(b) => b.on_missing_payload(args),
        }
    }

    fn build_empty_payload(
        &self,
        config: reth_basic_payload_builder::PayloadConfig<EthPayloadBuilderAttributes>,
    ) -> Result<EthBuiltPayload, reth_payload_builder_primitives::PayloadBuilderError> {
        match self {
            Self::Sequential(b) => b.build_empty_payload(config),
            Self::Parallel(b) => b.build_empty_payload(config),
        }
    }
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for QmdbPayloadBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = reth_evm::NextBlockEnvAttributes,
        > + 'static,
    Types::Payload: PayloadTypes<
        BuiltPayload = EthBuiltPayload,
        PayloadAttributes = EthPayloadAttributes,
        PayloadBuilderAttributes = EthPayloadBuilderAttributes,
    >,
{
    type PayloadBuilder = QmdbPayloadBuilderInner<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let chain = ctx.chain_spec().chain();
        let gas_limit = conf.gas_limit_for(chain);

        info!(
            target: "reth::cli",
            parallel_exec = self.parallel_exec,
            "Payload builder configured"
        );

        let builder_config = EthereumBuilderConfig::new()
            .with_gas_limit(gas_limit)
            .with_max_blobs_per_block(conf.max_blobs_per_block())
            .with_extra_data(conf.extra_data_bytes());

        if self.parallel_exec {
            info!(target: "reth::cli", "Using PARALLEL payload builder (true parallel execution)");
            let mut builder = parallel_payload::ParallelPayloadBuilder::new(
                ctx.provider().clone(),
                pool,
                evm_config,
                builder_config,
            );
            if let Some(store) = self.qmdb_store {
                builder = builder.with_qmdb_store(store);
            }
            Ok(QmdbPayloadBuilderInner::Parallel(builder))
        } else {
            Ok(QmdbPayloadBuilderInner::Sequential(
                reth_ethereum_payload_builder::EthereumPayloadBuilder::new(
                    ctx.provider().clone(),
                    pool,
                    evm_config,
                    builder_config.with_parallel_exec(false),
                ),
            ))
        }
    }
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = reth_ethereum_cli::Cli::<EthereumChainSpecParser, QmdbArgs>::parse()
        .run(|builder, qmdb_args| async move {
            info!(
                target: "reth::cli",
                parallel_exec = qmdb_args.parallel_exec,
                "Launching QMDB node (L1 mode)"
            );

            // ---------------------------------------------------------------
            // QMDB initialization
            // ---------------------------------------------------------------

            let qmdb_path = std::env::var("QMDB_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| builder.config().datadir().data_dir().join("qmdb"));

            if qmdb_path.exists() {
                info!(target: "reth::cli", path = %qmdb_path.display(), "Removing existing QMDB directory");
                std::fs::remove_dir_all(&qmdb_path)?;
            }

            info!(target: "reth::cli", path = %qmdb_path.display(), "Initializing QMDB store");
            let qmdb_store = QmdbStore::new(&qmdb_path);

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
            // If the parallel payload builder already submitted this block, skip the duplicate.
            // Otherwise, send to flusher thread for async processing.
            let store_for_commit = qmdb_store.clone();
            let on_canonical_commit = Box::new(move |_block_number, _block_hash, bundle: &revm_database::BundleState| {
                if !store_for_commit.skip_if_already_committed() {
                    store_for_commit.commit_bundle_async(bundle.clone());
                }
            });

            // State provider override: account/storage reads go through QMDB.
            let store_for_override = qmdb_store.clone();
            let state_override: reth_provider::providers::StateProviderOverride =
                Arc::new(move |_requested_hash, default_provider| {
                    Box::new(xlayer_qmdb_provider::QmdbStateProvider::with_fallback(
                        store_for_override.clone(),
                        default_provider,
                    ))
                });

            // ---------------------------------------------------------------
            // Engine tree configuration
            // ---------------------------------------------------------------

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

            // ---------------------------------------------------------------
            // Launch with EthereumNode + custom payload builder
            // ---------------------------------------------------------------

            let NodeHandle { node: _node, node_exit_future } = builder
                .with_types::<EthereumNode>()
                .with_components(
                    EthereumNode::components().payload(BasicPayloadServiceBuilder::new(
                        QmdbPayloadBuilder {
                            parallel_exec: qmdb_args.parallel_exec,
                            qmdb_store: if qmdb_args.parallel_exec {
                                Some(qmdb_store.clone())
                            } else {
                                None
                            },
                        },
                    )),
                )
                .with_add_ons(EthereumNode::default().add_ons())
                .launch_with(DebugNodeLauncher::new(launcher))
                .await?;

            info!(target: "reth::cli", "QMDB node launched successfully (L1 mode)");

            node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
