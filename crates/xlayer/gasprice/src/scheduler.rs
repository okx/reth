//! XLayer gas price scheduler
//!
//! Manages periodic gas price updates and handles the scheduling logic
//! for different gas price calculation strategies.

use alloy_consensus::BlockHeader;
use alloy_primitives::U256;
use parking_lot::RwLock;
use reth_primitives_traits::{Block as BlockTrait, BlockBody as BlockBodyTrait};
use reth_provider::{BlockNumReader, BlockReader, BlockReaderIdExt};
use reth_rpc_eth_api::{helpers::{LoadFee, pricer::GasPriceCacheTrait}, RpcNodeCore};
use reth_transaction_pool::TransactionPool;
use std::sync::Arc;
use tokio::time::interval;

use reth_rpc_eth_api::helpers::pricer::L2GasPricer;

use crate::{utils, DEFAULT_XLAYER_PRICE};

/// XLayer gas price scheduler
///
/// Handles periodic updates of gas prices based on:
/// - L2 gas price changes (from EthApi's shared GasPriceOracle)
/// - Network congestion
/// - Configured update period
///
/// The scheduler is generic over EthApi to access:
/// - GasPriceOracle for tip cap suggestions (shared with RPC)
/// - Provider for blockchain state
/// - Transaction pool for pending tx count
pub struct XLayerScheduler<EthApi = ()> {
    /// Gas price calculation strategy
    pricer: Arc<dyn L2GasPricer>,
    /// EthApi for accessing oracle, provider, and pool
    eth_api: Option<EthApi>,
    /// Whether the scheduler is running
    is_running: RwLock<bool>,
    /// Default gas price
    default_gas_price: U256,
    /// Update period
    update_period: std::time::Duration,
    /// Congestion threshold
    congestion_threshold: i32,
}

impl<EthApi> XLayerScheduler<EthApi> {
    /// Creates a new XLayer scheduler without EthApi
    /// This is useful for testing or when EthApi access is not needed
    pub fn new(
        pricer: Arc<dyn L2GasPricer>,
        default_gas_price: Option<U256>,
        update_period: Option<std::time::Duration>,
        congestion_threshold: Option<i32>,
    ) -> Self {
        Self {
            pricer,
            eth_api: None,
            is_running: RwLock::new(false),
            default_gas_price: default_gas_price.unwrap_or(U256::from(DEFAULT_XLAYER_PRICE)),
            update_period: update_period.unwrap_or(std::time::Duration::from_secs(10)),
            congestion_threshold: congestion_threshold.unwrap_or(100),
        }
    }

    /// Creates a new XLayer scheduler with EthApi
    /// 
    /// The EthApi provides access to:
    /// - GasPriceOracle (shared with RPC for consistent tip cap suggestions)
    /// - Provider (for reading blockchain data)
    /// - Pool (for checking pending transactions)
    pub fn with_eth_api(
        pricer: Arc<dyn L2GasPricer>,
        eth_api: EthApi,
        default_gas_price: Option<U256>,
        update_period: Option<std::time::Duration>,
        congestion_threshold: Option<i32>,
    ) -> Self {
        Self {
            pricer,
            eth_api: Some(eth_api),
            is_running: RwLock::new(false),
            default_gas_price: default_gas_price.unwrap_or(U256::from(DEFAULT_XLAYER_PRICE)),
            update_period: update_period.unwrap_or(std::time::Duration::from_secs(10)),
            congestion_threshold: congestion_threshold.unwrap_or(100),
        }
    }

    /// Legacy constructor for backward compatibility with FullNodeComponents
    pub fn with_components(
        pricer: Arc<dyn L2GasPricer>,
        eth_api: EthApi,
        default_gas_price: Option<U256>,
        update_period: Option<std::time::Duration>,
        congestion_threshold: Option<i32>,
    ) -> Self {
        Self::with_eth_api(pricer, eth_api, default_gas_price, update_period, congestion_threshold)
    }

    /// Gets the gas price cache
    pub fn get_gas_cache(&self) -> Arc<dyn GasPriceCacheTrait> {
        self.pricer.get_gas_cache()
    }

    /// Stops the gas price scheduler by setting is_running to false
    pub fn stop(&self) {
        *self.is_running.write() = false;
        tracing::info!("XLayer gas price scheduler stop requested");
    }
}

// Implementation for schedulers with EthApi
impl<EthApi> XLayerScheduler<EthApi>
where
    EthApi: RpcNodeCore + LoadFee + Clone + Send + Sync + 'static,
{
    /// Runs the scheduler loop
    ///
    /// This should be spawned as a background task.
    /// Initializes the scheduler and runs the update loop.
    pub async fn run(&self) {
        // Initialize scheduler
        {
            let mut is_running = self.is_running.write();
            if *is_running {
                return;
            }
            *is_running = true;
        }

        // Set initial gas price
        let cache = self.pricer.get_gas_cache();
        cache.set_latest(self.default_gas_price);
        cache.set_latest_raw_gp(self.default_gas_price);

        // Initialize update interval
        let mut update_interval = interval(self.update_period);
        update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!("XLayer gas price scheduler started");

        // Main update loop
        loop {
            // Check if stop was requested
            if !*self.is_running.read() {
                break;
            }

            // Wait for next interval tick (no lock held during await)
            update_interval.tick().await;

            // Check again after await (in case stop was called during tick)
            if !*self.is_running.read() {
                break;
            }

            // Perform update cycle
            self.tick().await;
        }

        tracing::info!("Stopping l2 gas price suggester...");
    }

    /// Runs one update cycle
    ///
    /// This should be called periodically by the main scheduler loop
    async fn tick(&self) {
        // For default mode, we don't need to update based on L1 gas price
        // The gas price is always the configured default value
        
        // Update dynamic gas price based on congestion if EthApi is available
        if self.eth_api.is_some() {
            // TODO: add l1gp fetching logic for follower and fixed modes
            // let l1_gas_price = self.get_l1_gas_price().await;
            // if let Ok(l1_gas_price) = l1_gas_price {
            //     self.pricer.update_gas_price_avg(l1_gas_price);
            //     self.pricer.get_gas_cache().set_latest_raw_gp(self.pricer.get_last_raw_gp());
            // } else {
            //     tracing::warn!("Failed to get L1 gas price, using default");
            //     return;
            // }

            self.update_dynamic_gp().await;
        } else {
            tracing::debug!("EthApi not available, skipping dynamic gas price update");
        }
    }

    /// Updates dynamic gas price based on network congestion
    async fn update_dynamic_gp(&self) {
        let cache = self.pricer.get_gas_cache();

        // Get L2 gas price (base fee + tip cap) from shared GasPriceOracle
        let l2_gas_price = match self.get_l2_gas_price().await {
            Ok(price) => price,
            Err(err) => {
                tracing::error!("error getting L2 gas price: {}", err);
                return;
            }
        };

        // Check if L2 gas price is less than default, if so use default
        let mut gas_result = if l2_gas_price < self.default_gas_price {
            tracing::debug!(
                "GasPriceOracle suggested gas price is less than xlayer default, setting to xlayer default, suggestedGasPrice={}, default={}",
                l2_gas_price,
                self.default_gas_price
            );
            self.default_gas_price
        } else {
            l2_gas_price
        };

        // Get raw gas price (recommended gas price)
        let raw_gp = self.pricer.get_last_raw_gp();

        // Check if gas_result is less than raw_gp, if so use raw_gp
        if gas_result < raw_gp {
            tracing::debug!(
                "gasResult is less than rgp, setting gasResult to recommendedGasPrice, gasResult={}, recommendedGasPrice={}",
                gas_result,
                raw_gp
            );
            gas_result = raw_gp;
        }

        // Check if network is congested
        let is_congested = self.is_congested().await;

        if !is_congested {
            // If not congested, use average of raw GP and current result
            tracing::debug!(
                "not congested, setting gasResult to avg of recommendedGasPrice and suggestGasPrice, recommendedGasPrice={}, gasResult={}",
                raw_gp,
                gas_result
            );
            gas_result = utils::avg_price(raw_gp, gas_result);
        }

        cache.set_latest(gas_result);
        tracing::info!("Updated gas price: {}", gas_result);
    }

    /// Checks if the network is congested
    ///
    /// Returns false if EthApi is not available or on error
    async fn is_congested(&self) -> bool {
        // If EthApi is not available, return false
        let Some(ref eth_api) = self.eth_api else {
            return false;
        };

        // Get latest block transaction count
        let latest_block_tx_num = match get_latest_block_tx_num(eth_api).await {
            Ok(count) => count,
            Err(err) => {
                tracing::warn!(?err, "Failed to get latest block transaction count");
                return false;
            }
        };

        tracing::debug!(latest_block_tx_num = %latest_block_tx_num, "Latest block transaction count");

        // Check if latest block is empty
        // op-stack will have at least 1 tx (DepositTx) in the latest block
        let is_latest_block_empty = latest_block_tx_num <= 1;

        // Get pending transaction count
        let pending_count = get_pending_tx_count(eth_api);

        tracing::debug!(pending_count = %pending_count, "Pending transaction count");

        // Get congestion threshold from scheduler config
        let is_pending_tx_congested = pending_count >= self.congestion_threshold as usize;

        // Network is congested if latest block is not empty AND pending tx count exceeds threshold
        !is_latest_block_empty && is_pending_tx_congested
    }

    /// Gets the L2 gas price (base fee + tip cap)
    /// 
    /// Uses the shared GasPriceOracle from EthApi to get the tip cap,
    /// ensuring consistency with RPC gas price estimates.
    async fn get_l2_gas_price(&self) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        let Some(ref eth_api) = self.eth_api else {
            tracing::error!("gasOracle is not available");
            return Err("gasOracle is not available".into());
        };

        // Get tip cap from the shared GasPriceOracle
        let tip_cap = eth_api.gas_oracle().suggest_tip_cap().await
            .map_err(|e| {
                tracing::error!("error SuggestTipCap: {}", e);
                Box::new(e) as Box<dyn std::error::Error + Send + Sync>
            })?;

        // Get latest header for base fee
        let header = eth_api.provider().latest_header()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .ok_or_else(|| {
                tracing::error!("baseFee is not available");
                "baseFee is not available"
            })?;

        let base_fee = header.base_fee_per_gas().unwrap_or_default();

        // L2 gas price = base fee + tip cap
        Ok(U256::from(base_fee) + tip_cap)
    }
}

/// Gets the transaction count from the latest block
async fn get_latest_block_tx_num<E>(eth_api: &E) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
where
    E: RpcNodeCore,
{
    let provider = eth_api.provider();
    
    // Get the best (latest) block number
    let best_number = provider.best_block_number()
        .map_err(|e| format!("Failed to get best block number: {}", e))?;
    
    // Get the block with that number
    let block = provider.block_by_number(best_number)
        .map_err(|e| format!("Failed to get block by number: {}", e))?
        .ok_or_else(|| format!("Block {} not found", best_number))?;
    
    // Return the transaction count
    Ok(block.body().transaction_count())
}

/// Gets the pending transaction count from the pool
fn get_pending_tx_count<E>(eth_api: &E) -> usize
where
    E: RpcNodeCore,
{
    let pool = eth_api.pool();
    let (pending, _queued) = pool.pending_and_queued_txn_count();
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default::DefaultGasPricer;

    #[test]
    fn test_scheduler_stop() {
        let pricer = Arc::new(DefaultGasPricer::new(None));
        let scheduler = XLayerScheduler::<()>::new(pricer, None, None, None);

        // Test that stop can be called
        scheduler.stop();
        
        // Verify the scheduler is marked as not running
        assert!(!*scheduler.is_running.read());
    }

    #[test]
    fn test_scheduler_creation() {
        let pricer = Arc::new(DefaultGasPricer::new(None));
        let scheduler = XLayerScheduler::<()>::new(pricer, None, None, Some(100));

        // Ensure scheduler can be created
        assert_eq!(scheduler.get_gas_cache().get_latest(), alloy_primitives::U256::ZERO);
    }
}
