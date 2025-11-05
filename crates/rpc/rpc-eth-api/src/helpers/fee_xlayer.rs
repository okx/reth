//! XLayer-specific fee-related functions for the [`EthApiServer`](crate::EthApiServer) trait.
//!
//! This module extends [`EthFees`] with XLayer-specific functionality, including
//! the ability to set and manage an L2 gas price pricer.

use super::{EthFees, LoadBlock};
use crate::helpers::pricer::L2GasPricer;
use crate::FromEthApiError;
use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use alloy_rpc_types_eth::FeeHistory;
use futures::Future;
use parking_lot::RwLock;
use reth_storage_api::BlockReaderIdExt;
use std::sync::Arc;
use tracing::debug;

/// XLayer-specific fee-related functions that extend [`EthFees`].
///
/// This trait wraps [`EthFees`] and adds the ability to set and manage
/// an L2 gas price pricer for XLayer-specific gas price calculations.
pub trait XLayerFees: EthFees {
    /// Sets the L2 gas price pricer.
    ///
    /// This allows setting the pricer at runtime, without requiring it
    /// to be set during initialization.
    fn set_pricer(&self, pricer: Arc<dyn L2GasPricer>);

    /// Gets the current L2 gas price pricer, if set.
    fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>>;

    /// Gets the base fee from the latest header.
    fn get_base_fee(&self) -> Result<u64, Self::Error>
    where
        Self: LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        let header = BlockReaderIdExt::latest_header(self.provider()).map_err(Self::Error::from_eth_err)?;
        Ok(header.as_ref().and_then(|h| h.header().base_fee_per_gas()).unwrap_or_default())
    }

    /// Gets the XLayer max priority fee (gas_price - base_fee).
    /// Returns 0 if pricer is not set.
    fn get_xlayer_max_priority_fee(&self) -> Result<U256, Self::Error>
    where
        Self: LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        if let Some(pricer) = self.get_pricer() {
            let gas_price = pricer.get_gas_cache().get_latest();
            let base_fee = self.get_base_fee()?;
            let tipcap = gas_price.saturating_sub(U256::from(base_fee));
            Ok(tipcap)
        } else {
            Ok(U256::ZERO)
        }
    }

    /// Returns a suggestion for a gas price for legacy transactions.
    ///
    /// For XLayer: returns the latest gas price from the pricer cache.
    /// For non-XLayer: forwards to [`EthFees::gas_price`].
    fn gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock,
    {
        async move {
            debug!(target: "rpc::eth::xlayer", "XLayerFees::gas_price called");
            
            if let Some(pricer) = self.get_pricer() {
                let gas_price = pricer.get_gas_cache().get_latest();
                debug!(target: "rpc::eth::xlayer", ?gas_price, "XLayerFees::gas_price: using XLayer gas price");
                return Ok(gas_price);
            }
            
            EthFees::gas_price(self).await
        }
    }

    /// Returns a suggestion for a base fee for blob transactions.
    fn blob_base_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock,
    {
        async move {
            debug!(target: "rpc::eth::xlayer", "XLayerFees::blob_base_fee called");
            EthFees::blob_base_fee(self).await
        }
    }

    /// Returns a suggestion for the priority fee (the tip).
    ///
    /// For XLayer: returns the latest gas price minus base fee from the pricer cache.
    /// For non-XLayer: forwards to [`EthFees::suggested_priority_fee`].
    fn suggested_priority_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: 'static + LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        async move {
            debug!(target: "rpc::eth::xlayer", "XLayerFees::suggested_priority_fee called");
            
            if let Some(_pricer) = self.get_pricer() {
                let tipcap = self.get_xlayer_max_priority_fee()?;
                debug!(target: "rpc::eth::xlayer", ?tipcap, "XLayerFees::suggested_priority_fee: using XLayer max priority fee");
                return Ok(tipcap);
            }
            
            EthFees::suggested_priority_fee(self).await
        }
    }

    /// Reports the fee history, for the given amount of blocks, up until the given newest block.
    ///
    /// For XLayer: adjusts rewards to ensure reward + baseFee >= latest_gas_price.
    /// For non-XLayer: forwards to [`EthFees::fee_history`].
    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> impl Future<Output = Result<FeeHistory, Self::Error>> + Send
    where
        Self: 'static + LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        async move {
            debug!(
                target: "rpc::eth::xlayer",
                block_count,
                ?newest_block,
                ?reward_percentiles,
                "XLayerFees::fee_history called"
            );
            
            let mut fee_history = EthFees::fee_history(self, block_count, newest_block, reward_percentiles.clone()).await?;
            
            if let Some(pricer) = self.get_pricer() {
                if let Some(ref mut rewards) = fee_history.reward {
                    let latest_gas_price = pricer.get_gas_cache().get_latest();
                    let latest_gas_price_u128 = latest_gas_price.to::<u128>();
                    
                    debug!(
                        target: "rpc::eth::xlayer",
                        ?latest_gas_price,
                        "XLayerFees::fee_history: adjusting rewards to ensure reward + baseFee >= latest_gas_price"
                    );
                    
                    for (block_idx, block_rewards) in rewards.iter_mut().enumerate() {
                        let base_fee = fee_history.base_fee_per_gas.get(block_idx).copied().unwrap_or(0u128);
                        
                        for reward in block_rewards.iter_mut() {
                            let old_reward = *reward;
                            let min_reward = latest_gas_price_u128.saturating_sub(base_fee);
                            *reward = old_reward.max(min_reward);
                        }
                    }
                }
            }
            
            Ok(fee_history)
        }
    }

    /// Returns the minimum gas price for XLayer transactions.
    ///
    /// For XLayer: returns the minimum raw gas price from recent history.
    /// For non-XLayer chains: returns the base fee or zero for pre-EIP-1559 chains.
    fn min_gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        async move {
            debug!(target: "rpc::eth::xlayer", "XLayerFees::min_gas_price called");
            
            if let Some(pricer) = self.get_pricer() {
                let min_gp = pricer.get_gas_cache().get_min_raw_gp_recent();
                debug!(target: "rpc::eth::xlayer", ?min_gp, "XLayerFees::min_gas_price: using XLayer min gas price");
                return Ok(min_gp);
            }
            
            let base_fee = self.get_base_fee()?;
            if base_fee > 0 {
                Ok(U256::from(base_fee))
            } else {
                Ok(U256::ZERO)
            }
        }
    }
}

/// Helper type for storing the pricer in a thread-safe, optional way.
pub type PricerStorage = Arc<RwLock<Option<Arc<dyn L2GasPricer>>>>;


