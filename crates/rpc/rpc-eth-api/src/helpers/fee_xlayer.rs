//! XLayer-specific fee extensions for [`EthFees`].
//!
//! Provides L2 gas price management, sequencer forwarding, and XLayer-specific
//! fee calculation logic.

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

// Re-export for use in implementations
pub use alloy_json_rpc::{RpcRecv, RpcSend};

/// XLayer fee extensions with L2 gas pricer and sequencer forwarding support.
///
/// Extends [`EthFees`] with:
/// - L2 gas price pricer management
/// - RPC request forwarding to sequencer
/// - XLayer-specific fee calculation logic
///
/// All methods have default implementations. Implementations only need to override
/// what they require.
pub trait XLayerFees: EthFees {
    /// Sequencer client for forwarding RPC requests.
    ///
    /// Must implement:
    /// ```ignore
    /// async fn request<Params: RpcSend, Resp: RpcRecv>(
    ///     &self, method: &str, params: Params
    /// ) -> Result<Resp, Error>
    /// ```
    ///
    /// Use `()` if sequencer forwarding is not needed.
    type SequencerClient: Clone + Send + Sync;

    /// Sets the L2 gas price pricer at runtime.
    fn set_pricer(&self, _pricer: Arc<dyn L2GasPricer>) {}


    /// Returns the current L2 gas price pricer, if configured.
    fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>> {
        None
    }

    /// Returns the sequencer client for RPC forwarding, if configured.
    fn sequencer_client(&self) -> Option<&Self::SequencerClient> {
        None
    }

    /// Forwards an RPC request to the sequencer.
    ///
    /// Returns `Ok(Some(result))` on success, `Ok(None)` if forwarding is unavailable or fails.
    fn forward_to_sequencer<Params, Resp>(
        &self,
        _method: &str,
        _params: Params,
    ) -> impl Future<Output = Result<Option<Resp>, Self::Error>> + Send
    where
        Params: RpcSend,
        Resp: RpcRecv,
        Self: Sized,
    {
        async move { Ok(None) }
    }

    /// Returns the base fee from the latest block header.
    fn get_base_fee(&self) -> Result<u64, Self::Error>
    where
        Self: LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        let header = BlockReaderIdExt::latest_header(self.provider()).map_err(Self::Error::from_eth_err)?;
        Ok(header.as_ref().and_then(|h| h.header().base_fee_per_gas()).unwrap_or_default())
    }

    /// Returns the XLayer max priority fee (gas_price - base_fee), or zero if no pricer.
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

    /// Returns suggested gas price for legacy transactions.
    ///
    /// Priority: sequencer > pricer > [`EthFees::gas_price`]
    fn gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock + 'static,
    {
        async move {
            if self.sequencer_client().is_some() {
                tracing::trace!("gas_price forwarding to sequencer");
                if let Ok(Some(gas_price)) = self.forward_to_sequencer::<(), U256>("eth_gasPrice", ()).await {
                    tracing::trace!("gas_price received from sequencer: {}", gas_price);
                    return Ok(gas_price);
                }
            }
            
            if let Some(pricer) = self.get_pricer() {
                tracing::trace!("gas_price from local pricer: {}", pricer.get_gas_cache().get_latest());
                return Ok(pricer.get_gas_cache().get_latest());
            }
            
            EthFees::gas_price(self).await
        }
    }

    /// Returns suggested base fee for blob transactions.
    ///
    /// Priority: sequencer > [`EthFees::blob_base_fee`]
    fn blob_base_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock + 'static,
    {
        async move {
            if self.sequencer_client().is_some() {
                tracing::trace!("blob_base_fee forwarding to sequencer");
                if let Ok(Some(blob_base_fee)) = self.forward_to_sequencer::<(), U256>("eth_blobBaseFee", ()).await {
                    tracing::trace!("blob_base_fee received from sequencer: {}", blob_base_fee);
                    return Ok(blob_base_fee);
                }
            }
            
            EthFees::blob_base_fee(self).await
        }
    }

    /// Returns suggested priority fee (tip).
    ///
    /// Priority: sequencer > XLayer max priority fee > [`EthFees::suggested_priority_fee`]
    fn suggested_priority_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: 'static + LoadBlock,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        async move {
            if self.sequencer_client().is_some() {
                tracing::trace!("suggested_priority_fee forwarding to sequencer");
                if let Ok(Some(priority_fee)) = self.forward_to_sequencer::<(), U256>("eth_maxPriorityFeePerGas", ()).await {
                    tracing::trace!("suggested_priority_fee received from sequencer: {}", priority_fee);
                    return Ok(priority_fee);
                }
            }
            
            if self.get_pricer().is_some() {
                tracing::trace!("suggested_priority_fee from local pricer");
                return self.get_xlayer_max_priority_fee();
            }
            
            EthFees::suggested_priority_fee(self).await
        }
    }

    /// Returns fee history for the specified block range.
    ///
    /// When sequencer is configured, forwards the request. Otherwise, returns
    /// [`EthFees::fee_history`] with XLayer adjustments if pricer is set
    /// (ensures `reward + baseFee >= latest_gas_price`).
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
            if self.sequencer_client().is_some() {
                tracing::trace!("fee_history forwarding to sequencer");
                if let Ok(Some(fee_history)) = self.forward_to_sequencer::<(u64, BlockNumberOrTag, Option<Vec<f64>>), FeeHistory>(
                    "eth_feeHistory",
                    (block_count, newest_block, reward_percentiles.clone())
                ).await {
                    tracing::trace!("fee_history received from sequencer: {:?}", fee_history);
                    return Ok(fee_history);
                }
            }
            
            let mut fee_history = EthFees::fee_history(self, block_count, newest_block, reward_percentiles.clone()).await?;
            
            if let Some(pricer) = self.get_pricer() {
                if let Some(ref mut rewards) = fee_history.reward {
                    let latest_gas_price_u128 = pricer.get_gas_cache().get_latest().to::<u128>();
                    
                    for (block_idx, block_rewards) in rewards.iter_mut().enumerate() {
                        let base_fee = fee_history.base_fee_per_gas.get(block_idx).copied().unwrap_or(0u128);
                        let min_reward = latest_gas_price_u128.saturating_sub(base_fee);
                        
                        for reward in block_rewards.iter_mut() {
                            *reward = (*reward).max(min_reward);
                        }
                    }
                }
            }
            
            Ok(fee_history)
        }
    }

    /// Returns the minimum acceptable gas price.
    ///
    /// Priority: sequencer > XLayer min from recent history > base fee > zero
    fn min_gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock + 'static,
        Self::Provider: BlockReaderIdExt,
        Self::Error: FromEthApiError,
    {
        async move {
            if self.sequencer_client().is_some() {
                tracing::trace!("min_gas_price forwarding to sequencer");
                if let Ok(Some(min_gas_price)) = self.forward_to_sequencer::<(), U256>("eth_gasPrice", ()).await {
                    tracing::trace!("min_gas_price received from sequencer: {}", min_gas_price);
                    return Ok(min_gas_price);
                }
            }
            
            if let Some(pricer) = self.get_pricer() {
                tracing::trace!("min_gas_price from local pricer: {}", pricer.get_gas_cache().get_min_raw_gp_recent());
                return Ok(pricer.get_gas_cache().get_min_raw_gp_recent());
            }
            
            let base_fee = self.get_base_fee()?;
            Ok(if base_fee > 0 { U256::from(base_fee) } else { U256::ZERO })
        }
    }
}

/// Helper type for storing the pricer in a thread-safe, optional way.
pub type PricerStorage = Arc<RwLock<Option<Arc<dyn L2GasPricer>>>>;


