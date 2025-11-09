//! XLayer-specific node functionality for Optimism.

use crate::XLayerGasPriceArgs;
use reth_node_api::FullNodeComponents;
use reth_node_builder::rpc::RpcRegistry;
use reth_optimism_forks::OpHardforks;
use reth_rpc_api::eth::EthApiTypes;
use reth_tasks::TaskExecutor;
use reth_tracing::tracing::info;
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPoolTransaction, PoolTransaction,
    TransactionPool, TransactionValidationTaskExecutor,
};
use reth_xlayer_txpool::XLayerTransactionValidator;
use reth_optimism_txpool::OpPooledTx;
use std::sync::Arc;

/// Extension trait for accessing XLayer validator from the pool.
pub(crate) trait XLayerValidatorAccess {
    type Provider;
    type Transaction: PoolTransaction;
    
    /// Get the XLayer validator from the pool.
    fn get_xlayer_validator(&self) -> Arc<XLayerTransactionValidator<Self::Provider, Self::Transaction>>;
}

/// Implement the extension trait for the specific Pool type used in OpNode.
impl<Client, Tx> XLayerValidatorAccess for reth_transaction_pool::Pool<
    TransactionValidationTaskExecutor<XLayerTransactionValidator<Client, Tx>>,
    CoinbaseTipOrdering<Tx>,
    DiskFileBlobStore,
>
where
    Client: reth_provider::ChainSpecProvider + reth_provider::StateProviderFactory + reth_provider::BlockReaderIdExt + Send + Sync + 'static,
    Client::ChainSpec: OpHardforks,
    Tx: EthPoolTransaction + OpPooledTx,
{
    type Provider = Client;
    type Transaction = Tx;
    
    fn get_xlayer_validator(&self) -> Arc<XLayerTransactionValidator<Client, Tx>> {
        self.validator().validator_arc().clone()
    }
}

/// Initialize XLayer gas price controller
///
/// This function sets up the gas price suggester, scheduler, and spawns the background task
/// for managing XLayer gas prices.
pub(crate) fn initialize_xlayer_gas_price_controller<Node, EthApi>(
    gas_price: &XLayerGasPriceArgs,
    registry: &mut RpcRegistry<Node, EthApi>,
    task_executor: TaskExecutor,
    validator: Arc<XLayerTransactionValidator<Node::Provider, <Node::Pool as TransactionPool>::Transaction>>,
) where
    Node: FullNodeComponents,
    Node::Pool: TransactionPool,
    EthApi: EthApiTypes + reth_rpc_eth_api::helpers::XLayerFees + reth_rpc_eth_api::helpers::LegacyRpc + Clone,
{
    info!(?gas_price, "Initializing XLayer gas price scheduler");
    
    // Create gas price suggester directly from args
    let pricer = reth_xlayer_gasprice::suggester::new_l2_gas_price_suggester(gas_price);
    
    // Get EthApi to share with scheduler
    let eth_api = registry.eth_api().clone();
    
    // Set pricer to XLayerFees
    // OpEthApi implements XLayerFees, so we can call set_pricer
    eth_api.set_pricer(pricer.clone());
    
    // Set pricer to XLayerTransactionValidator
    validator.set_pricer(pricer.clone());
    
    // Create scheduler with EthApi (which contains the shared GasPriceOracle)
    let scheduler = std::sync::Arc::new(
        reth_xlayer_gasprice::scheduler::XLayerScheduler::with_eth_api(
            pricer,
            eth_api,
            gas_price.default,
            gas_price.update_period,
            gas_price.congestion_threshold,
        )
    );
    
    // Spawn background task - run() will initialize and start the scheduler
    task_executor.spawn_critical(
        "xlayer-gas-price-scheduler",
        Box::pin(async move {
            scheduler.run().await;
        })
    );
    
    info!(target: "reth::cli", "XLayer gas price scheduler initialized");
}

