//! Contains common `reth` arguments

use std::path::Path;
use alloy_primitives::B256;
use alloy_genesis::Genesis;
use reth_db::{Database, transaction::DbTx};
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_config::{config::EtlConfig, Config};
use reth_consensus::noop::NoopConsensus;
use reth_db::{init_db, open_db_read_only, DatabaseEnv};
use reth_db_common::init::init_genesis;
use reth_downloaders::{bodies::noop::NoopBodiesDownloader, headers::noop::NoopHeaderDownloader};
use reth_eth_wire::NetPrimitivesFor;
use reth_evm::{noop::NoopEvmConfig, ConfigureEvm};
use reth_network::NetworkEventListenerProvider;
use reth_node_api::FullNodeTypesAdapter;
use reth_node_builder::{
    Node, NodeComponents, NodeComponentsBuilder, NodeTypes, NodeTypesWithDBAdapter,
};
use reth_node_core::{
    args::{DatabaseArgs, DatadirArgs},
    dirs::{ChainPath, DataDirPath},
};
use reth_provider::{
    providers::{BlockchainProvider, NodeTypesForProvider, StaticFileProvider},
    ProviderFactory, StaticFileProviderFactory,
};
use reth_stages::{sets::DefaultStages, Pipeline, PipelineTarget};
use reth_static_file::StaticFileProducer;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Struct to hold config and datadir paths
#[derive(Debug, Parser)]
pub struct EnvironmentArgs<C: ChainSpecParser> {
    /// Parameters for datadir configuration
    #[command(flatten)]
    pub datadir: DatadirArgs,

    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    ///
    /// If not specified, the chain will be loaded from the database (requires prior initialization).
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = C::help_message(),
        value_parser = C::parser(),
        global = true,
        required = false
    )]
    pub chain: Option<Arc<C::ChainSpec>>,

    /// All database related arguments
    #[command(flatten)]
    pub db: DatabaseArgs,
}

impl<C: ChainSpecParser> EnvironmentArgs<C> {
    /// Load chain spec from database using .chain-info file.
    ///
    /// This function breaks the circular dependency between needing the chain to determine
    /// the datadir path and needing the datadir path to load the chain from the database.
    fn load_chain_from_db<N>(
        datadir_base: &Path,
        is_user_specified: bool,
    ) -> eyre::Result<Arc<N::ChainSpec>>
    where
        N: CliNodeTypes,
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        use reth_node_core::dirs::ChainInfo;

        // Step 1: Read .chain-info to get chain_id
        let chain_info = ChainInfo::read(datadir_base)?;
        info!(target: "reth::cli",
            chain_id = chain_info.chain_id,
            genesis_hash = %chain_info.genesis_hash,
            "Loading chain from database"
        );

        // Step 2: Determine db path
        // If user specified datadir, db is directly under it: /path/db
        // If using default datadir, db is under chain_id subdirectory: ~/.local/share/reth/<chain_id>/db
        let db_path = if is_user_specified {
            // User specified path: /data/op-reth/db
            datadir_base.join("db")
        } else {
            // Default path: ~/.local/share/reth/196/db
            datadir_base.join(chain_info.chain_id.to_string()).join("db")
        };

        // Step 3: Open database (read-only)
        let db = Arc::new(open_db_read_only(&db_path, Default::default())?);
        let tx = db.tx()?;

        // Step 4: Read minimal genesis JSON (WITHOUT alloc, only ~2KB!)
        let genesis_bytes = tx
            .get::<reth_db_api::tables::GenesisConfig>(chain_info.genesis_hash)?
            .ok_or_else(|| {
                eyre::eyre!(
                    "Genesis config not found in database for hash: {}. \
                     Please specify --chain parameter or run init first.",
                    chain_info.genesis_hash
                )
            })?;

        info!(target: "reth::cli",
            genesis_size = genesis_bytes.len(),
            "Read minimal genesis from database"
        );

        // Step 5: Deserialize (fast, only ~2KB!)
        let minimal_genesis_json = String::from_utf8(genesis_bytes)
            .map_err(|e| eyre::eyre!("Invalid UTF-8 in genesis JSON: {}", e))?;
        let _genesis: Genesis = serde_json::from_str(&minimal_genesis_json)?;

        // Step 6: Convert to ChainSpec (fast, no alloc processing!)
        // Use the same conversion logic as when loading from file
        // C::parse returns Arc<ChainSpec> already
        let chain_spec = C::parse(&minimal_genesis_json)?;

        // Step 7: Verify consistency
        if chain_spec.chain().id() != chain_info.chain_id {
            return Err(eyre::eyre!(
                "Chain ID mismatch: .chain-info={}, genesis={}",
                chain_info.chain_id,
                chain_spec.chain().id()
            ))
        }

        if chain_spec.genesis_hash() != chain_info.genesis_hash {
            return Err(eyre::eyre!(
                "Genesis hash mismatch: .chain-info={}, computed={}",
                chain_info.genesis_hash,
                chain_spec.genesis_hash()
            ))
        }

        info!(target: "reth::cli",
            chain_id = chain_info.chain_id,
            "Chain loaded successfully from database"
        );

        Ok(chain_spec)
    }

    /// Initializes environment according to [`AccessRights`] and returns an instance of
    /// [`Environment`].
    pub fn init<N: CliNodeTypes>(&self, access: AccessRights) -> eyre::Result<Environment<N>>
    where
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        // Determine the chain to use: from parameter or database
        let chain = if let Some(chain) = &self.chain {
            // Use provided chain (--chain parameter)
            chain.clone()
        } else {
            // Try to load from database
            let datadir_base = self.datadir.resolve_datadir_base()?;
            // Check if user specified a datadir (vs using default)
            let is_user_specified = self.datadir.datadir.is_some();
            Self::load_chain_from_db::<N>(&datadir_base, is_user_specified)?
        };

        let data_dir = self.datadir.clone().resolve_datadir(chain.chain());
        let db_path = data_dir.db();
        let sf_path = data_dir.static_files();

        if access.is_read_write() {
            reth_fs_util::create_dir_all(&db_path)?;
            reth_fs_util::create_dir_all(&sf_path)?;
        }

        let config_path = self.config.clone().unwrap_or_else(|| data_dir.config());

        let mut config = Config::from_path(config_path)
            .inspect_err(
                |err| warn!(target: "reth::cli", %err, "Failed to load config file, using default"),
            )
            .unwrap_or_default();

        // Make sure ETL doesn't default to /tmp/, but to whatever datadir is set to
        if config.stages.etl.dir.is_none() {
            config.stages.etl.dir = Some(EtlConfig::from_datadir(data_dir.data_dir()));
        }
        if config.stages.era.folder.is_none() {
            config.stages.era = config.stages.era.with_datadir(data_dir.data_dir());
        }

        info!(target: "reth::cli", ?db_path, ?sf_path, "Opening storage");
        let (db, sfp) = match access {
            AccessRights::RW => (
                Arc::new(init_db(db_path, self.db.database_args())?),
                StaticFileProvider::read_write(sf_path)?,
            ),
            AccessRights::RO => (
                Arc::new(open_db_read_only(&db_path, self.db.database_args())?),
                StaticFileProvider::read_only(sf_path, false)?,
            ),
        };

        let provider_factory = self.create_provider_factory(&config, db, sfp, chain.clone())?;
        if access.is_read_write() {
            debug!(target: "reth::cli", chain=%chain.chain(), genesis=?chain.genesis_hash(), "Initializing genesis");
            init_genesis(&provider_factory)?;

            // Write .chain-info file for future startups without --chain parameter
            use reth_node_core::dirs::ChainInfo;
            let chain_info = ChainInfo {
                chain_id: chain.chain().id(),
                genesis_hash: chain.genesis_hash(),
                genesis_block_number: chain.genesis().number.unwrap_or(0),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string(),
            };
            // Write to the actual data directory (works for both user-specified and default paths)
            // - User specified: /data/op-reth/.chain-info
            // - Default: ~/.local/share/reth/<chain_id>/.chain-info
            chain_info.write(data_dir.data_dir())?;
        }

        Ok(Environment { config, provider_factory, data_dir })
    }

    /// Returns a [`ProviderFactory`] after executing consistency checks.
    ///
    /// If it's a read-write environment and an issue is found, it will attempt to heal (including a
    /// pipeline unwind). Otherwise, it will print out a warning, advising the user to restart the
    /// node to heal.
    fn create_provider_factory<N: CliNodeTypes>(
        &self,
        config: &Config,
        db: Arc<DatabaseEnv>,
        static_file_provider: StaticFileProvider<N::Primitives>,
        chain: Arc<N::ChainSpec>,
    ) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>>
    where
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        let has_receipt_pruning = config.prune.has_receipts_pruning();
        let prune_modes = config.prune.segments.clone();
        let factory = ProviderFactory::<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>::new(
            db,
            chain.clone(),
            static_file_provider,
        )
        .with_prune_modes(prune_modes.clone()).with_genesis_block_number(chain.genesis().number.unwrap());

        // Check for consistency between database and static files.
        if let Some(unwind_target) = factory
            .static_file_provider()
            .check_consistency(&factory.provider()?, has_receipt_pruning)?
        {
            if factory.db_ref().is_read_only()? {
                warn!(target: "reth::cli", ?unwind_target, "Inconsistent storage. Restart node to heal.");
                return Ok(factory)
            }

            // Highly unlikely to happen, and given its destructive nature, it's better to panic
            // instead.
            assert_ne!(
                unwind_target,
                PipelineTarget::Unwind(0),
                "A static file <> database inconsistency was found that would trigger an unwind to block 0"
            );

            info!(target: "reth::cli", unwind_target = %unwind_target, "Executing an unwind after a failed storage consistency check.");

            let (_tip_tx, tip_rx) = watch::channel(B256::ZERO);

            // Builds and executes an unwind-only pipeline
            let mut pipeline = Pipeline::<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>::builder()
                .add_stages(DefaultStages::new(
                    factory.clone(),
                    tip_rx,
                    Arc::new(NoopConsensus::default()),
                    NoopHeaderDownloader::default(),
                    NoopBodiesDownloader::default(),
                    NoopEvmConfig::<N::Evm>::default(),
                    config.stages.clone(),
                    prune_modes.clone(),
                    None,
                ))
                .build(factory.clone(), StaticFileProducer::new(factory.clone(), prune_modes));

            // Move all applicable data from database to static files.
            pipeline.move_to_static_files()?;
            pipeline.unwind(unwind_target.unwind_target().expect("should exist"), None)?;
        }

        Ok(factory)
    }
}

/// Environment built from [`EnvironmentArgs`].
#[derive(Debug)]
pub struct Environment<N: NodeTypes> {
    /// Configuration for reth node
    pub config: Config,
    /// Provider factory.
    pub provider_factory: ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>,
    /// Datadir path.
    pub data_dir: ChainPath<DataDirPath>,
}

/// Environment access rights.
#[derive(Debug, Copy, Clone)]
pub enum AccessRights {
    /// Read-write access
    RW,
    /// Read-only access
    RO,
}

impl AccessRights {
    /// Returns `true` if it requires read-write access to the environment.
    pub const fn is_read_write(&self) -> bool {
        matches!(self, Self::RW)
    }
}

/// Helper alias to satisfy `FullNodeTypes` bound on [`Node`] trait generic.
type FullTypesAdapter<T> = FullNodeTypesAdapter<
    T,
    Arc<DatabaseEnv>,
    BlockchainProvider<NodeTypesWithDBAdapter<T, Arc<DatabaseEnv>>>,
>;

/// Trait for block headers that can be modified through CLI operations.
pub trait CliHeader {
    fn set_number(&mut self, number: u64);
}

impl CliHeader for alloy_consensus::Header {
    fn set_number(&mut self, number: u64) {
        self.number = number;
    }
}

/// Helper trait with a common set of requirements for the
/// [`NodeTypes`] in CLI.
pub trait CliNodeTypes: Node<FullTypesAdapter<Self>> + NodeTypesForProvider {
    type Evm: ConfigureEvm<Primitives = Self::Primitives>;
    type NetworkPrimitives: NetPrimitivesFor<Self::Primitives>;
}

impl<N> CliNodeTypes for N
where
    N: Node<FullTypesAdapter<Self>> + NodeTypesForProvider,
{
    type Evm = <<N::ComponentsBuilder as NodeComponentsBuilder<FullTypesAdapter<Self>>>::Components as NodeComponents<FullTypesAdapter<Self>>>::Evm;
    type NetworkPrimitives = <<<N::ComponentsBuilder as NodeComponentsBuilder<FullTypesAdapter<Self>>>::Components as NodeComponents<FullTypesAdapter<Self>>>::Network as NetworkEventListenerProvider>::Primitives;
}

type EvmFor<N> = <<<N as Node<FullTypesAdapter<N>>>::ComponentsBuilder as NodeComponentsBuilder<
    FullTypesAdapter<N>,
>>::Components as NodeComponents<FullTypesAdapter<N>>>::Evm;

type ConsensusFor<N> =
    <<<N as Node<FullTypesAdapter<N>>>::ComponentsBuilder as NodeComponentsBuilder<
        FullTypesAdapter<N>,
    >>::Components as NodeComponents<FullTypesAdapter<N>>>::Consensus;

/// Helper trait aggregating components required for the CLI.
pub trait CliNodeComponents<N: CliNodeTypes>: Send + Sync + 'static {
    /// Returns the configured EVM.
    fn evm_config(&self) -> &EvmFor<N>;
    /// Returns the consensus implementation.
    fn consensus(&self) -> &ConsensusFor<N>;
}

impl<N: CliNodeTypes> CliNodeComponents<N> for (EvmFor<N>, ConsensusFor<N>) {
    fn evm_config(&self) -> &EvmFor<N> {
        &self.0
    }

    fn consensus(&self) -> &ConsensusFor<N> {
        &self.1
    }
}

/// Helper trait alias for an [`FnOnce`] producing [`CliNodeComponents`].
pub trait CliComponentsBuilder<N: CliNodeTypes>:
    FnOnce(Arc<N::ChainSpec>) -> Self::Components + Send + Sync + 'static
{
    type Components: CliNodeComponents<N>;
}

impl<N: CliNodeTypes, F, Comp> CliComponentsBuilder<N> for F
where
    F: FnOnce(Arc<N::ChainSpec>) -> Comp + Send + Sync + 'static,
    Comp: CliNodeComponents<N>,
{
    type Components = Comp;
}
