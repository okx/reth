//! Transaction pool arguments

use crate::cli::config::RethTransactionPoolConfig;
use alloy_eips::eip1559::{ETHEREUM_BLOCK_GAS_LIMIT_30M, MIN_PROTOCOL_BASE_FEE};
use alloy_primitives::Address;
use clap::{builder::Resettable, Args};
use reth_cli_util::{parse_duration_from_secs_or_ms, parsers::format_duration_as_secs_or_ms};
use reth_transaction_pool::{
    blobstore::disk::DEFAULT_MAX_CACHED_BLOBS,
    maintain::MAX_QUEUED_TRANSACTION_LIFETIME,
    pool::{NEW_TX_LISTENER_BUFFER_SIZE, PENDING_TX_LISTENER_BUFFER_SIZE},
    validate::DEFAULT_MAX_TX_INPUT_BYTES,
    LocalTransactionConfig, PoolConfig, PriceBumpConfig, SubPoolLimit, DEFAULT_PRICE_BUMP,
    DEFAULT_TXPOOL_ADDITIONAL_VALIDATION_TASKS, MAX_NEW_PENDING_TXS_NOTIFICATIONS,
    REPLACE_BLOB_PRICE_BUMP, TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER,
    TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT, TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
};
use std::{path::PathBuf, sync::OnceLock, time::Duration};

/// Global static transaction pool defaults
static TXPOOL_DEFAULTS: OnceLock<DefaultTxPoolValues> = OnceLock::new();

/// Default values for transaction pool that can be customized
///
/// Global defaults can be set via [`DefaultTxPoolValues::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultTxPoolValues {
    pending_max_count: usize,
    pending_max_size: usize,
    basefee_max_count: usize,
    basefee_max_size: usize,
    queued_max_count: usize,
    queued_max_size: usize,
    blobpool_max_count: usize,
    blobpool_max_size: usize,
    blob_cache_size: Option<u32>,
    disable_blobs_support: bool,
    max_account_slots: usize,
    price_bump: u128,
    minimal_protocol_basefee: u64,
    minimum_priority_fee: Option<u128>,
    enforced_gas_limit: u64,
    max_tx_gas_limit: Option<u64>,
    blob_transaction_price_bump: u128,
    max_tx_input_bytes: usize,
    max_cached_entries: u32,
    no_locals: bool,
    locals: Vec<Address>,
    no_local_transactions_propagation: bool,
    additional_validation_tasks: usize,
    pending_tx_listener_buffer_size: usize,
    new_tx_listener_buffer_size: usize,
    max_new_pending_txs_notifications: usize,
    max_queued_lifetime: Duration,
    transactions_backup_path: Option<PathBuf>,
    disable_transactions_backup: bool,
    max_batch_size: usize,
    // Pre-warming configuration
    pre_warming_enabled: bool,
    pre_warming_num_workers: usize,
    pre_warming_simulation_timeout_ms: u64,
    pre_warming_cache_ttl_secs: u64,
    pre_warming_cache_max_entries: usize,
}

impl DefaultTxPoolValues {
    /// Initialize the global transaction pool defaults with this configuration
    pub fn try_init(self) -> Result<(), Self> {
        TXPOOL_DEFAULTS.set(self)
    }

    /// Get a reference to the global transaction pool defaults
    pub fn get_global() -> &'static Self {
        TXPOOL_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default pending sub-pool max transaction count
    pub const fn with_pending_max_count(mut self, v: usize) -> Self {
        self.pending_max_count = v;
        self
    }

    /// Set the default pending sub-pool max size in MB
    pub const fn with_pending_max_size(mut self, v: usize) -> Self {
        self.pending_max_size = v;
        self
    }

    /// Set the default basefee sub-pool max transaction count
    pub const fn with_basefee_max_count(mut self, v: usize) -> Self {
        self.basefee_max_count = v;
        self
    }

    /// Set the default basefee sub-pool max size in MB
    pub const fn with_basefee_max_size(mut self, v: usize) -> Self {
        self.basefee_max_size = v;
        self
    }

    /// Set the default queued sub-pool max transaction count
    pub const fn with_queued_max_count(mut self, v: usize) -> Self {
        self.queued_max_count = v;
        self
    }

    /// Set the default queued sub-pool max size in MB
    pub const fn with_queued_max_size(mut self, v: usize) -> Self {
        self.queued_max_size = v;
        self
    }

    /// Set the default blobpool max transaction count
    pub const fn with_blobpool_max_count(mut self, v: usize) -> Self {
        self.blobpool_max_count = v;
        self
    }

    /// Set the default blobpool max size in MB
    pub const fn with_blobpool_max_size(mut self, v: usize) -> Self {
        self.blobpool_max_size = v;
        self
    }

    /// Set the default blob cache size
    pub const fn with_blob_cache_size(mut self, v: Option<u32>) -> Self {
        self.blob_cache_size = v;
        self
    }

    /// Set whether to disable blob transaction support by default
    pub const fn with_disable_blobs_support(mut self, v: bool) -> Self {
        self.disable_blobs_support = v;
        self
    }

    /// Set the default max account slots
    pub const fn with_max_account_slots(mut self, v: usize) -> Self {
        self.max_account_slots = v;
        self
    }

    /// Set the default price bump percentage
    pub const fn with_price_bump(mut self, v: u128) -> Self {
        self.price_bump = v;
        self
    }

    /// Set the default minimal protocol base fee
    pub const fn with_minimal_protocol_basefee(mut self, v: u64) -> Self {
        self.minimal_protocol_basefee = v;
        self
    }

    /// Set the default minimum priority fee
    pub const fn with_minimum_priority_fee(mut self, v: Option<u128>) -> Self {
        self.minimum_priority_fee = v;
        self
    }

    /// Set the default enforced gas limit
    pub const fn with_enforced_gas_limit(mut self, v: u64) -> Self {
        self.enforced_gas_limit = v;
        self
    }

    /// Set the default max transaction gas limit
    pub const fn with_max_tx_gas_limit(mut self, v: Option<u64>) -> Self {
        self.max_tx_gas_limit = v;
        self
    }

    /// Set the default blob transaction price bump
    pub const fn with_blob_transaction_price_bump(mut self, v: u128) -> Self {
        self.blob_transaction_price_bump = v;
        self
    }

    /// Set the default max transaction input bytes
    pub const fn with_max_tx_input_bytes(mut self, v: usize) -> Self {
        self.max_tx_input_bytes = v;
        self
    }

    /// Set the default max cached entries
    pub const fn with_max_cached_entries(mut self, v: u32) -> Self {
        self.max_cached_entries = v;
        self
    }

    /// Set whether to disable local transaction exemptions by default
    pub const fn with_no_locals(mut self, v: bool) -> Self {
        self.no_locals = v;
        self
    }

    /// Set the default local addresses
    pub fn with_locals(mut self, v: Vec<Address>) -> Self {
        self.locals = v;
        self
    }

    /// Set whether to disable local transaction propagation by default
    pub const fn with_no_local_transactions_propagation(mut self, v: bool) -> Self {
        self.no_local_transactions_propagation = v;
        self
    }

    /// Set the default additional validation tasks
    pub const fn with_additional_validation_tasks(mut self, v: usize) -> Self {
        self.additional_validation_tasks = v;
        self
    }

    /// Set the default pending transaction listener buffer size
    pub const fn with_pending_tx_listener_buffer_size(mut self, v: usize) -> Self {
        self.pending_tx_listener_buffer_size = v;
        self
    }

    /// Set the default new transaction listener buffer size
    pub const fn with_new_tx_listener_buffer_size(mut self, v: usize) -> Self {
        self.new_tx_listener_buffer_size = v;
        self
    }

    /// Set the default max new pending transactions notifications
    pub const fn with_max_new_pending_txs_notifications(mut self, v: usize) -> Self {
        self.max_new_pending_txs_notifications = v;
        self
    }

    /// Set the default max queued lifetime
    pub const fn with_max_queued_lifetime(mut self, v: Duration) -> Self {
        self.max_queued_lifetime = v;
        self
    }

    /// Set the default transactions backup path
    pub fn with_transactions_backup_path(mut self, v: Option<PathBuf>) -> Self {
        self.transactions_backup_path = v;
        self
    }

    /// Set whether to disable transaction backup by default
    pub const fn with_disable_transactions_backup(mut self, v: bool) -> Self {
        self.disable_transactions_backup = v;
        self
    }

    /// Set the default max batch size
    pub const fn with_max_batch_size(mut self, v: usize) -> Self {
        self.max_batch_size = v;
        self
    }

    /// Set whether pre-warming is enabled by default
    pub const fn with_pre_warming_enabled(mut self, v: bool) -> Self {
        self.pre_warming_enabled = v;
        self
    }

    /// Set the default number of pre-warming workers
    pub const fn with_pre_warming_num_workers(mut self, v: usize) -> Self {
        self.pre_warming_num_workers = v;
        self
    }

    /// Set the default pre-warming simulation timeout in milliseconds
    pub const fn with_pre_warming_simulation_timeout_ms(mut self, v: u64) -> Self {
        self.pre_warming_simulation_timeout_ms = v;
        self
    }

    /// Set the default pre-warming cache TTL in seconds
    pub const fn with_pre_warming_cache_ttl_secs(mut self, v: u64) -> Self {
        self.pre_warming_cache_ttl_secs = v;
        self
    }

    /// Set the default pre-warming cache max entries
    pub const fn with_pre_warming_cache_max_entries(mut self, v: usize) -> Self {
        self.pre_warming_cache_max_entries = v;
        self
    }
}

impl Default for DefaultTxPoolValues {
    fn default() -> Self {
        Self {
            pending_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            pending_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            basefee_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            basefee_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            queued_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            queued_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            blobpool_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            blobpool_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            blob_cache_size: None,
            disable_blobs_support: false,
            max_account_slots: TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER,
            price_bump: DEFAULT_PRICE_BUMP,
            minimal_protocol_basefee: MIN_PROTOCOL_BASE_FEE,
            minimum_priority_fee: None,
            enforced_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
            max_tx_gas_limit: None,
            blob_transaction_price_bump: REPLACE_BLOB_PRICE_BUMP,
            max_tx_input_bytes: DEFAULT_MAX_TX_INPUT_BYTES,
            max_cached_entries: DEFAULT_MAX_CACHED_BLOBS,
            no_locals: false,
            locals: Vec::new(),
            no_local_transactions_propagation: false,
            additional_validation_tasks: DEFAULT_TXPOOL_ADDITIONAL_VALIDATION_TASKS,
            pending_tx_listener_buffer_size: PENDING_TX_LISTENER_BUFFER_SIZE,
            new_tx_listener_buffer_size: NEW_TX_LISTENER_BUFFER_SIZE,
            max_new_pending_txs_notifications: MAX_NEW_PENDING_TXS_NOTIFICATIONS,
            max_queued_lifetime: MAX_QUEUED_TRANSACTION_LIFETIME,
            transactions_backup_path: None,
            disable_transactions_backup: false,
            max_batch_size: 1,
            // Pre-warming defaults (experimental, disabled by default)
            pre_warming_enabled: false,
            pre_warming_num_workers: 4,
            pre_warming_simulation_timeout_ms: 100,
            pre_warming_cache_ttl_secs: 60,
            pre_warming_cache_max_entries: 10_000,
        }
    }
}

/// Parameters for debugging purposes
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "TxPool")]
pub struct TxPoolArgs {
    /// Max number of transaction in the pending sub-pool.
    #[arg(long = "txpool.pending-max-count", alias = "txpool.pending_max_count", default_value_t = DefaultTxPoolValues::get_global().pending_max_count)]
    pub pending_max_count: usize,
    /// Max size of the pending sub-pool in megabytes.
    #[arg(long = "txpool.pending-max-size", alias = "txpool.pending_max_size", default_value_t = DefaultTxPoolValues::get_global().pending_max_size)]
    pub pending_max_size: usize,

    /// Max number of transaction in the basefee sub-pool
    #[arg(long = "txpool.basefee-max-count", alias = "txpool.basefee_max_count", default_value_t = DefaultTxPoolValues::get_global().basefee_max_count)]
    pub basefee_max_count: usize,
    /// Max size of the basefee sub-pool in megabytes.
    #[arg(long = "txpool.basefee-max-size", alias = "txpool.basefee_max_size", default_value_t = DefaultTxPoolValues::get_global().basefee_max_size)]
    pub basefee_max_size: usize,

    /// Max number of transaction in the queued sub-pool
    #[arg(long = "txpool.queued-max-count", alias = "txpool.queued_max_count", default_value_t = DefaultTxPoolValues::get_global().queued_max_count)]
    pub queued_max_count: usize,
    /// Max size of the queued sub-pool in megabytes.
    #[arg(long = "txpool.queued-max-size", alias = "txpool.queued_max_size", default_value_t = DefaultTxPoolValues::get_global().queued_max_size)]
    pub queued_max_size: usize,

    /// Max number of transaction in the blobpool
    #[arg(long = "txpool.blobpool-max-count", alias = "txpool.blobpool_max_count", default_value_t = DefaultTxPoolValues::get_global().blobpool_max_count)]
    pub blobpool_max_count: usize,
    /// Max size of the blobpool in megabytes.
    #[arg(long = "txpool.blobpool-max-size", alias = "txpool.blobpool_max_size", default_value_t = DefaultTxPoolValues::get_global().blobpool_max_size)]
    pub blobpool_max_size: usize,

    /// Max number of entries for the in memory cache of the blob store.
    #[arg(long = "txpool.blob-cache-size", alias = "txpool.blob_cache_size", default_value = Resettable::from(DefaultTxPoolValues::get_global().blob_cache_size.map(|v| v.to_string().into())))]
    pub blob_cache_size: Option<u32>,

    /// Disable EIP-4844 blob transaction support
    #[arg(long = "txpool.disable-blobs-support", alias = "txpool.disable_blobs_support", default_value_t = DefaultTxPoolValues::get_global().disable_blobs_support, conflicts_with_all = ["blobpool_max_count", "blobpool_max_size", "blob_cache_size", "blob_transaction_price_bump"])]
    pub disable_blobs_support: bool,

    /// Max number of executable transaction slots guaranteed per account
    #[arg(long = "txpool.max-account-slots", alias = "txpool.max_account_slots", default_value_t = DefaultTxPoolValues::get_global().max_account_slots)]
    pub max_account_slots: usize,

    /// Price bump (in %) for the transaction pool underpriced check.
    #[arg(long = "txpool.pricebump", default_value_t = DefaultTxPoolValues::get_global().price_bump)]
    pub price_bump: u128,

    /// Minimum base fee required by the protocol.
    #[arg(long = "txpool.minimal-protocol-fee", default_value_t = DefaultTxPoolValues::get_global().minimal_protocol_basefee)]
    pub minimal_protocol_basefee: u64,

    /// Minimum priority fee required for transaction acceptance into the pool.
    /// Transactions with priority fee below this value will be rejected.
    #[arg(long = "txpool.minimum-priority-fee", default_value = Resettable::from(DefaultTxPoolValues::get_global().minimum_priority_fee.map(|v| v.to_string().into())))]
    pub minimum_priority_fee: Option<u128>,

    /// The default enforced gas limit for transactions entering the pool
    #[arg(long = "txpool.gas-limit", default_value_t = DefaultTxPoolValues::get_global().enforced_gas_limit)]
    pub enforced_gas_limit: u64,

    /// Maximum gas limit for individual transactions. Transactions exceeding this limit will be
    /// rejected by the transaction pool
    #[arg(long = "txpool.max-tx-gas", default_value = Resettable::from(DefaultTxPoolValues::get_global().max_tx_gas_limit.map(|v| v.to_string().into())))]
    pub max_tx_gas_limit: Option<u64>,

    /// Price bump percentage to replace an already existing blob transaction
    #[arg(long = "blobpool.pricebump", default_value_t = DefaultTxPoolValues::get_global().blob_transaction_price_bump)]
    pub blob_transaction_price_bump: u128,

    /// Max size in bytes of a single transaction allowed to enter the pool
    #[arg(long = "txpool.max-tx-input-bytes", alias = "txpool.max_tx_input_bytes", default_value_t = DefaultTxPoolValues::get_global().max_tx_input_bytes)]
    pub max_tx_input_bytes: usize,

    /// The maximum number of blobs to keep in the in memory blob cache.
    #[arg(long = "txpool.max-cached-entries", alias = "txpool.max_cached_entries", default_value_t = DefaultTxPoolValues::get_global().max_cached_entries)]
    pub max_cached_entries: u32,

    /// Flag to disable local transaction exemptions.
    #[arg(long = "txpool.nolocals", default_value_t = DefaultTxPoolValues::get_global().no_locals)]
    pub no_locals: bool,
    /// Flag to allow certain addresses as local.
    #[arg(long = "txpool.locals", default_values = DefaultTxPoolValues::get_global().locals.iter().map(ToString::to_string))]
    pub locals: Vec<Address>,
    /// Flag to toggle local transaction propagation.
    #[arg(long = "txpool.no-local-transactions-propagation", default_value_t = DefaultTxPoolValues::get_global().no_local_transactions_propagation)]
    pub no_local_transactions_propagation: bool,

    /// Number of additional transaction validation tasks to spawn.
    #[arg(long = "txpool.additional-validation-tasks", alias = "txpool.additional_validation_tasks", default_value_t = DefaultTxPoolValues::get_global().additional_validation_tasks)]
    pub additional_validation_tasks: usize,

    /// Maximum number of pending transactions from the network to buffer
    #[arg(long = "txpool.max-pending-txns", alias = "txpool.max_pending_txns", default_value_t = DefaultTxPoolValues::get_global().pending_tx_listener_buffer_size)]
    pub pending_tx_listener_buffer_size: usize,

    /// Maximum number of new transactions to buffer
    #[arg(long = "txpool.max-new-txns", alias = "txpool.max_new_txns", default_value_t = DefaultTxPoolValues::get_global().new_tx_listener_buffer_size)]
    pub new_tx_listener_buffer_size: usize,

    /// How many new pending transactions to buffer and send to in progress pending transaction
    /// iterators.
    #[arg(long = "txpool.max-new-pending-txs-notifications", alias = "txpool.max-new-pending-txs-notifications", default_value_t = DefaultTxPoolValues::get_global().max_new_pending_txs_notifications)]
    pub max_new_pending_txs_notifications: usize,

    /// Maximum amount of time non-executable transaction are queued.
    #[arg(long = "txpool.lifetime", value_parser = parse_duration_from_secs_or_ms, value_name = "DURATION", default_value = format_duration_as_secs_or_ms(DefaultTxPoolValues::get_global().max_queued_lifetime))]
    pub max_queued_lifetime: Duration,

    /// Path to store the local transaction backup at, to survive node restarts.
    #[arg(long = "txpool.transactions-backup", alias = "txpool.journal", value_name = "PATH", default_value = Resettable::from(DefaultTxPoolValues::get_global().transactions_backup_path.as_ref().map(|v| v.to_string_lossy().into())))]
    pub transactions_backup_path: Option<PathBuf>,

    /// Disables transaction backup to disk on node shutdown.
    #[arg(
        long = "txpool.disable-transactions-backup",
        alias = "txpool.disable-journal",
        conflicts_with = "transactions_backup_path",
        default_value_t = DefaultTxPoolValues::get_global().disable_transactions_backup
    )]
    pub disable_transactions_backup: bool,

    /// Max batch size for transaction pool insertions
    #[arg(long = "txpool.max-batch-size", default_value_t = DefaultTxPoolValues::get_global().max_batch_size)]
    pub max_batch_size: usize,

    // Pre-warming configuration (experimental feature)
    /// Enable pre-warming simulation for cache optimization.
    /// When enabled, transactions are simulated in background to discover state keys
    /// that will be accessed during execution, allowing parallel pre-fetching.
    #[arg(long = "txpool.pre-warming", default_value_t = DefaultTxPoolValues::get_global().pre_warming_enabled)]
    pub pre_warming_enabled: bool,

    /// Number of pre-warming simulation workers.
    /// More workers enable more parallel simulation but increase CPU/memory usage.
    #[arg(long = "txpool.pre-warming-workers", default_value_t = DefaultTxPoolValues::get_global().pre_warming_num_workers)]
    pub pre_warming_num_workers: usize,

    /// Maximum simulation time in milliseconds before timeout.
    /// Prevents hanging on pathological transactions.
    #[arg(long = "txpool.pre-warming-timeout-ms", default_value_t = DefaultTxPoolValues::get_global().pre_warming_simulation_timeout_ms)]
    pub pre_warming_simulation_timeout_ms: u64,

    /// Cache TTL in seconds for pre-warmed keys.
    /// Keys older than this are considered stale and evicted.
    #[arg(long = "txpool.pre-warming-cache-ttl", default_value_t = DefaultTxPoolValues::get_global().pre_warming_cache_ttl_secs)]
    pub pre_warming_cache_ttl_secs: u64,

    /// Maximum number of entries in pre-warming cache.
    /// Prevents unbounded memory growth. Each entry is ~500 bytes.
    #[arg(long = "txpool.pre-warming-cache-max", default_value_t = DefaultTxPoolValues::get_global().pre_warming_cache_max_entries)]
    pub pre_warming_cache_max_entries: usize,
}

impl TxPoolArgs {
    /// Sets the minimal protocol base fee to 0, effectively disabling checks that enforce that a
    /// transaction's fee must be higher than the [`MIN_PROTOCOL_BASE_FEE`] which is the lowest
    /// value the ethereum EIP-1559 base fee can reach.
    pub const fn with_disabled_protocol_base_fee(self) -> Self {
        self.with_protocol_base_fee(0)
    }

    /// Configures the minimal protocol base fee that should be enforced.
    ///
    /// Ethereum's EIP-1559 base fee can't drop below [`MIN_PROTOCOL_BASE_FEE`] hence this is
    /// enforced by default in the pool.
    pub const fn with_protocol_base_fee(mut self, protocol_base_fee: u64) -> Self {
        self.minimal_protocol_basefee = protocol_base_fee;
        self
    }
}

impl Default for TxPoolArgs {
    fn default() -> Self {
        let DefaultTxPoolValues {
            pending_max_count,
            pending_max_size,
            basefee_max_count,
            basefee_max_size,
            queued_max_count,
            queued_max_size,
            blobpool_max_count,
            blobpool_max_size,
            blob_cache_size,
            disable_blobs_support,
            max_account_slots,
            price_bump,
            minimal_protocol_basefee,
            minimum_priority_fee,
            enforced_gas_limit,
            max_tx_gas_limit,
            blob_transaction_price_bump,
            max_tx_input_bytes,
            max_cached_entries,
            no_locals,
            locals,
            no_local_transactions_propagation,
            additional_validation_tasks,
            pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size,
            max_new_pending_txs_notifications,
            max_queued_lifetime,
            transactions_backup_path,
            disable_transactions_backup,
            max_batch_size,
            pre_warming_enabled,
            pre_warming_num_workers,
            pre_warming_simulation_timeout_ms,
            pre_warming_cache_ttl_secs,
            pre_warming_cache_max_entries,
        } = DefaultTxPoolValues::get_global().clone();
        Self {
            pending_max_count,
            pending_max_size,
            basefee_max_count,
            basefee_max_size,
            queued_max_count,
            queued_max_size,
            blobpool_max_count,
            blobpool_max_size,
            blob_cache_size,
            disable_blobs_support,
            max_account_slots,
            price_bump,
            minimal_protocol_basefee,
            minimum_priority_fee,
            enforced_gas_limit,
            max_tx_gas_limit,
            blob_transaction_price_bump,
            max_tx_input_bytes,
            max_cached_entries,
            no_locals,
            locals,
            no_local_transactions_propagation,
            additional_validation_tasks,
            pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size,
            max_new_pending_txs_notifications,
            max_queued_lifetime,
            transactions_backup_path,
            disable_transactions_backup,
            max_batch_size,
            pre_warming_enabled,
            pre_warming_num_workers,
            pre_warming_simulation_timeout_ms,
            pre_warming_cache_ttl_secs,
            pre_warming_cache_max_entries,
        }
    }
}

impl RethTransactionPoolConfig for TxPoolArgs {
    /// Returns transaction pool configuration.
    fn pool_config(&self) -> PoolConfig {
        let default_config = PoolConfig::default();
        PoolConfig {
            local_transactions_config: LocalTransactionConfig {
                no_exemptions: self.no_locals,
                local_addresses: self.locals.clone().into_iter().collect(),
                propagate_local_transactions: !self.no_local_transactions_propagation,
            },
            pending_limit: SubPoolLimit {
                max_txs: self.pending_max_count,
                max_size: self.pending_max_size.saturating_mul(1024 * 1024),
            },
            basefee_limit: SubPoolLimit {
                max_txs: self.basefee_max_count,
                max_size: self.basefee_max_size.saturating_mul(1024 * 1024),
            },
            queued_limit: SubPoolLimit {
                max_txs: self.queued_max_count,
                max_size: self.queued_max_size.saturating_mul(1024 * 1024),
            },
            blob_limit: SubPoolLimit {
                max_txs: self.blobpool_max_count,
                max_size: self.blobpool_max_size.saturating_mul(1024 * 1024),
            },
            blob_cache_size: self.blob_cache_size,
            max_account_slots: self.max_account_slots,
            price_bumps: PriceBumpConfig {
                default_price_bump: self.price_bump,
                replace_blob_tx_price_bump: self.blob_transaction_price_bump,
            },
            minimal_protocol_basefee: self.minimal_protocol_basefee,
            minimum_priority_fee: self.minimum_priority_fee,
            gas_limit: self.enforced_gas_limit,
            pending_tx_listener_buffer_size: self.pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size: self.new_tx_listener_buffer_size,
            max_new_pending_txs_notifications: self.max_new_pending_txs_notifications,
            max_queued_lifetime: self.max_queued_lifetime,
            max_inflight_delegated_slot_limit: default_config.max_inflight_delegated_slot_limit,
            #[cfg(feature = "pre-warming")]
            pre_warming: reth_transaction_pool::pre_warming::PreWarmingConfig {
                enabled: self.pre_warming_enabled,
                num_workers: self.pre_warming_num_workers,
                simulation_timeout: std::time::Duration::from_millis(self.pre_warming_simulation_timeout_ms),
                cache_ttl: std::time::Duration::from_secs(self.pre_warming_cache_ttl_secs),
                cache_max_entries: self.pre_warming_cache_max_entries,
                enable_metrics: true, // Always enable metrics when pre-warming is configured
            },
        }
    }

    /// Returns max batch size for transaction batch insertion.
    fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn txpool_args_default_sanity_test() {
        let default_args = TxPoolArgs::default();
        let args = CommandParser::<TxPoolArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn txpool_parse_max_tx_lifetime() {
        // Test with a custom duration
        let args =
            CommandParser::<TxPoolArgs>::parse_from(["reth", "--txpool.lifetime", "300"]).args;
        assert_eq!(args.max_queued_lifetime, Duration::from_secs(300));

        // Test with the default value
        let args = CommandParser::<TxPoolArgs>::parse_from(["reth"]).args;
        assert_eq!(args.max_queued_lifetime, Duration::from_secs(3 * 60 * 60)); // Default is 3h
    }

    #[test]
    fn txpool_parse_max_tx_lifetime_invalid() {
        let result =
            CommandParser::<TxPoolArgs>::try_parse_from(["reth", "--txpool.lifetime", "invalid"]);

        assert!(result.is_err(), "Expected an error for invalid duration");
    }

    #[test]
    fn txpool_args() {
        let args = TxPoolArgs {
            pending_max_count: 1000,
            pending_max_size: 200,
            basefee_max_count: 2000,
            basefee_max_size: 300,
            queued_max_count: 3000,
            queued_max_size: 400,
            blobpool_max_count: 4000,
            blobpool_max_size: 500,
            blob_cache_size: Some(100),
            disable_blobs_support: false,
            max_account_slots: 20,
            price_bump: 15,
            minimal_protocol_basefee: 1000000000,
            minimum_priority_fee: Some(2000000000),
            enforced_gas_limit: 40000000,
            max_tx_gas_limit: Some(50000000),
            blob_transaction_price_bump: 25,
            max_tx_input_bytes: 131072,
            max_cached_entries: 200,
            no_locals: true,
            locals: vec![
                address!("0x0000000000000000000000000000000000000001"),
                address!("0x0000000000000000000000000000000000000002"),
            ],
            no_local_transactions_propagation: true,
            additional_validation_tasks: 4,
            pending_tx_listener_buffer_size: 512,
            new_tx_listener_buffer_size: 256,
            max_new_pending_txs_notifications: 128,
            max_queued_lifetime: Duration::from_secs(7200),
            transactions_backup_path: Some(PathBuf::from("/tmp/txpool-backup")),
            disable_transactions_backup: false,
            max_batch_size: 10,
            // Pre-warming with custom values (enabled via flag presence)
            pre_warming_enabled: true,
            pre_warming_num_workers: 8,
            pre_warming_simulation_timeout_ms: 50,
            pre_warming_cache_ttl_secs: 30,
            pre_warming_cache_max_entries: 5000,
        };

        let parsed_args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pending-max-count",
            "1000",
            "--txpool.pending-max-size",
            "200",
            "--txpool.basefee-max-count",
            "2000",
            "--txpool.basefee-max-size",
            "300",
            "--txpool.queued-max-count",
            "3000",
            "--txpool.queued-max-size",
            "400",
            "--txpool.blobpool-max-count",
            "4000",
            "--txpool.blobpool-max-size",
            "500",
            "--txpool.blob-cache-size",
            "100",
            "--txpool.max-account-slots",
            "20",
            "--txpool.pricebump",
            "15",
            "--txpool.minimal-protocol-fee",
            "1000000000",
            "--txpool.minimum-priority-fee",
            "2000000000",
            "--txpool.gas-limit",
            "40000000",
            "--txpool.max-tx-gas",
            "50000000",
            "--blobpool.pricebump",
            "25",
            "--txpool.max-tx-input-bytes",
            "131072",
            "--txpool.max-cached-entries",
            "200",
            "--txpool.nolocals",
            "--txpool.locals",
            "0x0000000000000000000000000000000000000001",
            "--txpool.locals",
            "0x0000000000000000000000000000000000000002",
            "--txpool.no-local-transactions-propagation",
            "--txpool.additional-validation-tasks",
            "4",
            "--txpool.max-pending-txns",
            "512",
            "--txpool.max-new-txns",
            "256",
            "--txpool.max-new-pending-txs-notifications",
            "128",
            "--txpool.lifetime",
            "7200",
            "--txpool.transactions-backup",
            "/tmp/txpool-backup",
            "--txpool.max-batch-size",
            "10",
            // Pre-warming CLI arguments
            "--txpool.pre-warming",
            "--txpool.pre-warming-workers",
            "8",
            "--txpool.pre-warming-timeout-ms",
            "50",
            "--txpool.pre-warming-cache-ttl",
            "30",
            "--txpool.pre-warming-cache-max",
            "5000",
        ])
        .args;

        assert_eq!(parsed_args, args);
    }

    #[test]
    fn txpool_pre_warming_defaults() {
        // Test that pre-warming defaults are correctly applied
        let args = CommandParser::<TxPoolArgs>::parse_from(["reth"]).args;

        assert!(!args.pre_warming_enabled, "Pre-warming should be disabled by default");
        assert_eq!(args.pre_warming_num_workers, 4, "Default workers should be 4");
        assert_eq!(args.pre_warming_simulation_timeout_ms, 100, "Default timeout should be 100ms");
        assert_eq!(args.pre_warming_cache_ttl_secs, 60, "Default TTL should be 60 seconds");
        assert_eq!(args.pre_warming_cache_max_entries, 10_000, "Default max entries should be 10000");
    }

    #[test]
    fn txpool_pre_warming_enable_only() {
        // Test enabling pre-warming without changing other defaults
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming",
        ])
        .args;

        assert!(args.pre_warming_enabled);
        // Other values should remain at defaults
        assert_eq!(args.pre_warming_num_workers, 4);
        assert_eq!(args.pre_warming_simulation_timeout_ms, 100);
        assert_eq!(args.pre_warming_cache_ttl_secs, 60);
        assert_eq!(args.pre_warming_cache_max_entries, 10_000);
    }

    #[test]
    fn txpool_pre_warming_custom_workers() {
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-workers",
            "16",
        ])
        .args;

        assert_eq!(args.pre_warming_num_workers, 16);
    }

    #[test]
    fn txpool_pre_warming_custom_timeout() {
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-timeout-ms",
            "250",
        ])
        .args;

        assert_eq!(args.pre_warming_simulation_timeout_ms, 250);
    }

    #[test]
    fn txpool_pre_warming_custom_cache_ttl() {
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-cache-ttl",
            "120",
        ])
        .args;

        assert_eq!(args.pre_warming_cache_ttl_secs, 120);
    }

    #[test]
    fn txpool_pre_warming_custom_cache_max() {
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-cache-max",
            "50000",
        ])
        .args;

        assert_eq!(args.pre_warming_cache_max_entries, 50000);
    }

    #[test]
    fn txpool_pre_warming_all_custom() {
        // Test setting all pre-warming options together
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming",
            "--txpool.pre-warming-workers",
            "12",
            "--txpool.pre-warming-timeout-ms",
            "75",
            "--txpool.pre-warming-cache-ttl",
            "45",
            "--txpool.pre-warming-cache-max",
            "20000",
        ])
        .args;

        assert!(args.pre_warming_enabled);
        assert_eq!(args.pre_warming_num_workers, 12);
        assert_eq!(args.pre_warming_simulation_timeout_ms, 75);
        assert_eq!(args.pre_warming_cache_ttl_secs, 45);
        assert_eq!(args.pre_warming_cache_max_entries, 20000);
    }

    #[test]
    fn txpool_pre_warming_edge_case_zero_workers() {
        // Zero workers - should parse but may be handled specially by the pool
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-workers",
            "0",
        ])
        .args;

        assert_eq!(args.pre_warming_num_workers, 0);
    }

    #[test]
    fn txpool_pre_warming_edge_case_large_values() {
        // Test with large values
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-workers",
            "128",
            "--txpool.pre-warming-timeout-ms",
            "10000",
            "--txpool.pre-warming-cache-ttl",
            "3600",
            "--txpool.pre-warming-cache-max",
            "1000000",
        ])
        .args;

        assert_eq!(args.pre_warming_num_workers, 128);
        assert_eq!(args.pre_warming_simulation_timeout_ms, 10000);
        assert_eq!(args.pre_warming_cache_ttl_secs, 3600);
        assert_eq!(args.pre_warming_cache_max_entries, 1000000);
    }

    #[test]
    fn txpool_pre_warming_custom_values_without_enable_flag() {
        // Custom values without enabling pre-warming (default disabled)
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-workers",
            "8",
            "--txpool.pre-warming-cache-max",
            "5000",
        ])
        .args;

        assert!(!args.pre_warming_enabled);
        assert_eq!(args.pre_warming_num_workers, 8);
        assert_eq!(args.pre_warming_cache_max_entries, 5000);
    }

    #[test]
    fn txpool_pre_warming_one_worker() {
        // Minimum practical worker count
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming",
            "--txpool.pre-warming-workers",
            "1",
        ])
        .args;

        assert!(args.pre_warming_enabled);
        assert_eq!(args.pre_warming_num_workers, 1);
    }

    #[test]
    fn txpool_pre_warming_small_timeout() {
        // Very small timeout (1ms)
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-timeout-ms",
            "1",
        ])
        .args;

        assert_eq!(args.pre_warming_simulation_timeout_ms, 1);
    }

    #[test]
    fn txpool_pre_warming_small_cache() {
        // Minimum cache size
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-cache-max",
            "1",
        ])
        .args;

        assert_eq!(args.pre_warming_cache_max_entries, 1);
    }

    #[test]
    fn txpool_pre_warming_zero_ttl() {
        // Zero TTL (immediate eviction)
        let args = CommandParser::<TxPoolArgs>::parse_from([
            "reth",
            "--txpool.pre-warming-cache-ttl",
            "0",
        ])
        .args;

        assert_eq!(args.pre_warming_cache_ttl_secs, 0);
    }

    #[test]
    #[cfg(feature = "pre-warming")]
    fn txpool_pre_warming_pool_config_mapping() {
        // Test that CLI args correctly map to PoolConfig
        let args = TxPoolArgs {
            pre_warming_enabled: true,
            pre_warming_num_workers: 6,
            pre_warming_simulation_timeout_ms: 150,
            pre_warming_cache_ttl_secs: 90,
            pre_warming_cache_max_entries: 15000,
            ..Default::default()
        };

        let pool_config = args.pool_config();

        assert!(pool_config.pre_warming.enabled);
        assert_eq!(pool_config.pre_warming.num_workers, 6);
        assert_eq!(pool_config.pre_warming.simulation_timeout, Duration::from_millis(150));
        assert_eq!(pool_config.pre_warming.cache_ttl, Duration::from_secs(90));
        assert_eq!(pool_config.pre_warming.cache_max_entries, 15000);
        assert!(pool_config.pre_warming.enable_metrics); // Always true when configured
    }

    #[test]
    #[cfg(feature = "pre-warming")]
    fn txpool_pre_warming_pool_config_defaults() {
        // Test default PoolConfig mapping
        let args = TxPoolArgs::default();
        let pool_config = args.pool_config();

        assert!(!pool_config.pre_warming.enabled);
        assert_eq!(pool_config.pre_warming.num_workers, 4);
        assert_eq!(pool_config.pre_warming.simulation_timeout, Duration::from_millis(100));
        assert_eq!(pool_config.pre_warming.cache_ttl, Duration::from_secs(60));
        assert_eq!(pool_config.pre_warming.cache_max_entries, 10_000);
    }

    #[test]
    #[cfg(feature = "pre-warming")]
    fn txpool_pre_warming_pool_config_duration_conversion() {
        // Test that duration conversions are correct
        let args = TxPoolArgs {
            pre_warming_simulation_timeout_ms: 500,
            pre_warming_cache_ttl_secs: 120,
            ..Default::default()
        };

        let pool_config = args.pool_config();

        // Verify exact duration values
        assert_eq!(pool_config.pre_warming.simulation_timeout.as_millis(), 500);
        assert_eq!(pool_config.pre_warming.cache_ttl.as_secs(), 120);
    }
}
