use alloy_consensus::{Header as ConsensusHeader, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::merge::BEACON_NONCE;
use alloy_json_rpc::RpcObject;
use alloy_primitives::{Address, FixedBytes, U256};
use alloy_rpc_types_eth::{
    pubsub::{Params as AlloyParams, SubscriptionKind as AlloySubscriptionKind},
    Header,
};
use futures::StreamExt;
use jsonrpsee::{
    proc_macros::rpc, server::SubscriptionMessage, types::ErrorObject, PendingSubscriptionSink,
    SubscriptionSink,
};
use reth_optimism_flashblocks::{FlashBlock, PendingBlockRx, PendingFlashBlock};
use reth_primitives_traits::header::SealedHeader;
use reth_rpc::eth::pubsub::EthPubSub;
use reth_rpc_eth_api::pubsub::EthPubSubApiServer;
use reth_rpc_server_types::result::internal_rpc_err;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::{wrappers::WatchStream, Stream};
use tracing::info;

/// Extended subscription kind that wraps Alloy's `SubscriptionKind` and adds Optimism-specific
/// variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    /// Wraps all standard Alloy subscription kinds.
    Standard(AlloySubscriptionKind),
    /// Flashblocks subscription.
    ///
    /// Returns flashblocks as they are received from the sequencer.
    /// This is an Optimism-specific extension to the standard Ethereum subscription types.
    Flashblocks,
}

impl<'de> Deserialize<'de> for SubscriptionKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        match s.as_str() {
            "flashblocks" => Ok(SubscriptionKind::Flashblocks),
            "newHeads" => Ok(SubscriptionKind::Standard(AlloySubscriptionKind::NewHeads)),
            "logs" => Ok(SubscriptionKind::Standard(AlloySubscriptionKind::Logs)),
            "newPendingTransactions" => {
                Ok(SubscriptionKind::Standard(AlloySubscriptionKind::NewPendingTransactions))
            }
            "syncing" => Ok(SubscriptionKind::Standard(AlloySubscriptionKind::Syncing)),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["flashblocks", "newHeads", "logs", "newPendingTransactions", "syncing"],
            )),
        }
    }
}

impl Serialize for SubscriptionKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            SubscriptionKind::Standard(kind) => {
                // Serialize the standard kind as its string representation
                let s = match kind {
                    AlloySubscriptionKind::NewHeads => "newHeads",
                    AlloySubscriptionKind::Logs => "logs",
                    AlloySubscriptionKind::NewPendingTransactions => "newPendingTransactions",
                    AlloySubscriptionKind::Syncing => "syncing",
                };
                serializer.serialize_str(s)
            }
            SubscriptionKind::Flashblocks => serializer.serialize_str("flashblocks"),
        }
    }
}

impl SubscriptionKind {
    /// Returns `true` if this is a flashblocks subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Flashblocks)
    }

    /// Returns `true` if this is a `NewHeads` subscription.
    pub fn is_new_heads(&self) -> bool {
        matches!(self, Self::Standard(AlloySubscriptionKind::NewHeads))
    }

    /// Returns the inner standard subscription kind, if any.
    pub const fn as_standard(&self) -> Option<&AlloySubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(kind),
            Self::Flashblocks => None,
        }
    }
}

impl From<AlloySubscriptionKind> for SubscriptionKind {
    fn from(kind: AlloySubscriptionKind) -> Self {
        Self::Standard(kind)
    }
}

impl From<SubscriptionKind> for AlloySubscriptionKind {
    fn from(kind: SubscriptionKind) -> Self {
        match kind {
            SubscriptionKind::Standard(alloy_kind) => alloy_kind,
            SubscriptionKind::Flashblocks => {
                // Flashblocks is not a standard subscription kind, so we can't convert it
                // This should only be called when we know it's not Flashblocks
                unreachable!("Cannot convert Flashblocks to AlloySubscriptionKind")
            }
        }
    }
}

/// Extended params that wraps Alloy's `Params` and adds Optimism-specific variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    /// Standard Ethereum subscription params (for logs, etc.)
    Standard(AlloyParams),
    /// Flashblocks stream criteria
    StreamCriteria(StreamCriteria),
    /// No params
    None,
}

impl<'de> Deserialize<'de> for Params {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Try to deserialize as a generic JSON value first
        let value = serde_json::Value::deserialize(deserializer)?;

        // Try to parse as StreamCriteria first
        if let Ok(criteria) = serde_json::from_value::<StreamCriteria>(value.clone()) {
            return Ok(Params::StreamCriteria(criteria));
        }

        // Try to parse as standard Alloy Params
        if let Ok(standard_params) = serde_json::from_value::<AlloyParams>(value) {
            return Ok(Params::Standard(standard_params));
        }

        // If neither works, treat as None
        Ok(Params::None)
    }
}

impl Serialize for Params {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Params::Standard(params) => params.serialize(serializer),
            Params::StreamCriteria(criteria) => criteria.serialize(serializer),
            Params::None => serializer.serialize_none(),
        }
    }
}

impl Params {
    /// Returns the inner `StreamCriteria` if this is a `StreamCriteria` variant.
    pub fn as_stream_criteria(&self) -> Option<&StreamCriteria> {
        match self {
            Params::StreamCriteria(criteria) => Some(criteria),
            _ => None,
        }
    }

    /// Returns the inner standard `Params` if this is a `Standard` variant.
    pub fn as_standard(&self) -> Option<&AlloyParams> {
        match self {
            Params::Standard(params) => Some(params),
            _ => None,
        }
    }
}

/// Criteria for filtering and enriching flashblock subscription data.
///
/// This allows clients to customize what data is included in flashblock updates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCriteria {
    /// Include new block headers in the stream.
    #[serde(default)]
    pub new_heads: bool,

    /// Include extra transaction information (sender, gas used, etc.).
    #[serde(default)]
    pub transaction_extra_info: bool,

    /// Include transaction receipts.
    #[serde(default)]
    pub transaction_receipt: bool,

    /// Only include transactions involving these addresses (empty = all transactions).
    #[serde(default)]
    pub subscribed_addresses: Vec<Address>,
}

/// Enriched flashblock data returned to subscribers based on StreamCriteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedFlashblock<H> {
    /// Block header (if `new_heads` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Header<H>>,

    /// Filtered transactions with optional enrichment
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub transactions: Vec<EnrichedTransaction>,
}

/// Transaction data with optional enrichment based on StreamCriteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EnrichedTransaction {
    /// Transaction hash (always included)
    pub tx_hash: alloy_primitives::TxHash,

    /// Transaction data (if `transaction_extra_info` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_data: Option<serde_json::Value>,

    /// Transaction receipt (if `transaction_receipt` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<serde_json::Value>,
}

impl StreamCriteria {
    /// Creates a new `StreamCriteria` with all options disabled.
    pub const fn new() -> Self {
        Self {
            new_heads: false,
            transaction_extra_info: false,
            transaction_receipt: false,
            subscribed_addresses: Vec::new(),
        }
    }

    /// Creates criteria for receiving only block headers.
    pub fn headers_only() -> Self {
        Self { new_heads: true, ..Default::default() }
    }

    /// Creates criteria for receiving full transaction details.
    pub fn full_transactions() -> Self {
        Self {
            new_heads: true,
            transaction_extra_info: true,
            transaction_receipt: true,
            subscribed_addresses: Vec::new(),
        }
    }

    /// Returns `true` if any transaction-related fields are enabled.
    pub const fn includes_transactions(&self) -> bool {
        self.transaction_extra_info || self.transaction_receipt
    }

    /// Returns `true` if address filtering is enabled.
    pub fn has_address_filter(&self) -> bool {
        !self.subscribed_addresses.is_empty()
    }
}

/// Flashblocks pubsub RPC interface.
///
/// This trait provides the same interface as `EthPubSubApi` but with relaxed trait bounds
/// to allow implementation for `OpEthPubSub` without requiring `SignedTx = PoolConsensusTx<Pool>`.
///
/// Supports standard Ethereum subscriptions plus custom "flashblocks" subscriptions with
/// optional `StreamCriteria` for filtering.
#[rpc(server, namespace = "eth")]
pub trait FlashblocksPubSubApi<T: RpcObject> {
    /// Create an ethereum subscription for the given params
    ///
    /// # Parameters
    /// - `kind`: Subscription type ("flashblocks", "newHeads", "logs", etc.)
    /// - `params`: Optional parameters - for flashblocks, this can be `StreamCriteria`; for logs,
    ///   this is a `Filter`
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = alloy_rpc_types::pubsub::SubscriptionResult
    )]
    async fn subscribe(
        &self,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult;
}

pub struct OpEthPubSub<Eth, N: reth_primitives_traits::NodePrimitives> {
    /// Standard eth pubsub handler
    eth_pubsub: EthPubSub<Eth>,
    /// Pending block receiver from flashblocks, if available
    pending_block_rx: Option<PendingBlockRx<N>>,
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> Clone for OpEthPubSub<Eth, N>
where
    Eth: Clone,
{
    fn clone(&self) -> Self {
        Self {
            eth_pubsub: self.eth_pubsub.clone(),
            pending_block_rx: self.pending_block_rx.clone(),
        }
    }
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> std::fmt::Debug for OpEthPubSub<Eth, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpEthPubSub")
            .field("eth_pubsub", &self.eth_pubsub)
            .field("pending_block_rx", &self.pending_block_rx.is_some())
            .finish()
    }
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> OpEthPubSub<Eth, N> {
    /// Creates a new `OpEthPubSub` instance.
    pub fn new(eth_pubsub: EthPubSub<Eth>, pending_block_rx: Option<PendingBlockRx<N>>) -> Self {
        Self { eth_pubsub, pending_block_rx }
    }

    /// Returns a reference to the wrapped `EthPubSub`.
    pub fn eth_pubsub(&self) -> &EthPubSub<Eth> {
        &self.eth_pubsub
    }

    /// Converts this `OpEthPubSub` into an RPC module.
    ///
    /// This method uses `FlashblocksPubSubApiServer` which has relaxed trait bounds,
    /// allowing it to be used in contexts where `EthPubSubApiServer` bounds aren't satisfied.
    pub fn into_rpc(self) -> jsonrpsee::RpcModule<()>
    where
        Eth: reth_rpc_eth_api::EthApiTypes,
        OpEthPubSub<Eth, N>:
            FlashblocksPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>,
    {
        <OpEthPubSub<Eth, N> as FlashblocksPubSubApiServer<
            reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>,
        >>::into_rpc(self)
        .remove_context()
    }

    /// Returns a stream that yields all new RPC headers from flashblocks.
    ///
    /// This stream converts flashblocks to headers, filtering out any errors
    /// (e.g., when the broadcast stream lags).
    ///
    /// Returns `None` if flashblocks are not available.
    pub fn new_flashblocks_header_stream(
        &self,
    ) -> Option<impl Stream<Item = Header<N::BlockHeader>>> {
        self.pending_block_rx.as_ref().map(|pending_block_rx| {
            WatchStream::new(pending_block_rx.clone()).filter_map(|pending_block_opt| async move {
                pending_block_opt.and_then(|pending_block| {
                    extract_header_from_pending_block(&pending_block).ok()
                })
            })
        })
    }
}

#[async_trait::async_trait]
impl<Eth, N: reth_primitives_traits::NodePrimitives>
    FlashblocksPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>
    for OpEthPubSub<Eth, N>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<
            Provider: reth_storage_api::BlockNumReader
                          + reth_chain_state::CanonStateSubscriptions<Primitives = N>,
            Pool: reth_transaction_pool::TransactionPool,
        > + reth_rpc_eth_api::EthApiTypes<
            RpcConvert: reth_rpc_eth_api::RpcConvert<
                Primitives: reth_primitives_traits::NodePrimitives<
                    SignedTx = reth_transaction_pool::PoolConsensusTx<Eth::Pool>,
                >,
            >,
        > + 'static,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        info!("XXX line 163: {:?}, params: {:?}", kind, params);

        // Extract StreamCriteria from params if present
        let criteria =
            params.as_ref().and_then(|p| p.as_stream_criteria()).cloned().unwrap_or_default();
        info!("XXX using criteria: {:?}", criteria);

        match kind {
            SubscriptionKind::Flashblocks => {
                // Handle flashblocks subscription
                if let Some(pending_block_rx) = &self.pending_block_rx {
                    info!(
                        "XXX flashblocks subscription with pending_block_rx available, criteria: {:?}",
                        criteria
                    );
                    // TODO: Implement flashblocks-specific subscription that returns full
                    // FlashBlock data Use the criteria to filter/enrich the data
                    return self
                        .filter_flashblocks_stream(pending, pending_block_rx, &criteria)
                        .await;
                } else {
                    let err = internal_rpc_err("Flashblocks are not available on this node");
                    pending.accept().await?;
                    return Err(jsonrpsee::core::SubscriptionError::from(err));
                }
            }
            SubscriptionKind::Standard(alloy_kind) => {
                if matches!(alloy_kind, AlloySubscriptionKind::NewHeads) {
                    if let Some(pending_block_rx) = &self.pending_block_rx {
                        info!(
                            "XXX newHeads subscription with flashblocks available, criteria: {:?}",
                            criteria
                        );
                        return self
                            .subscribe_pending_blocks_as_headers(
                                pending,
                                pending_block_rx,
                                &criteria,
                            )
                            .await;
                    }
                }

                let standard_params = params.and_then(|p| p.as_standard().cloned());

                self.eth_pubsub.subscribe(pending, alloy_kind, standard_params).await?;
                Ok(())
            }
        }
    }
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> OpEthPubSub<Eth, N> {
    /// Parse `StreamCriteria` from the subscription params.
    ///
    /// The params can contain an optional `StreamCriteria` object as the first element.
    /// If not provided or parsing fails, returns a default criteria.
    fn parse_stream_criteria(params: &Option<Params>) -> StreamCriteria {
        params
            .as_ref()
            .and_then(|p| match p {
                // Extract StreamCriteria if present
                Params::StreamCriteria(criteria) => Some(criteria.clone()),
                // Standard Ethereum subscription params don't contain StreamCriteria
                Params::Standard(_) | Params::None => None,
            })
            .unwrap_or_default()
    }
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> OpEthPubSub<Eth, N>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<Provider: reth_storage_api::BlockNumReader>
        + reth_rpc_eth_api::EthApiTypes<
            RpcConvert: reth_rpc_eth_api::RpcConvert<
                Primitives: reth_primitives_traits::NodePrimitives,
            >,
        >,
{
    /// Subscribe to pending blocks and convert them to Header format for newHeads
    async fn subscribe_pending_blocks_as_headers(
        &self,
        pending: PendingSubscriptionSink,
        pending_block_rx: &PendingBlockRx<N>,
        _criteria: &StreamCriteria,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let pending_block_rx = pending_block_rx.clone();
        // Convert pending blocks stream to headers stream
        let headers_stream =
            WatchStream::new(pending_block_rx).filter_map(|pending_block_opt| async move {
                pending_block_opt.and_then(|pending_block| {
                    extract_header_from_pending_block(&pending_block).ok()
                })
            });
        let pinned_stream = Box::pin(headers_stream);

        tokio::spawn(async move {
            let _ = pipe_from_stream(sink, pinned_stream).await;
        });

        Ok(())
    }

    async fn filter_flashblocks_stream(
        &self,
        pending: PendingSubscriptionSink,
        pending_block_rx: &PendingBlockRx<N>,
        criteria: &StreamCriteria,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let pending_block_rx = pending_block_rx.clone();
        let criteria = criteria.clone();

        let flashblocks_stream =
            WatchStream::new(pending_block_rx).filter_map(move |pending_block_opt| {
                let criteria = criteria.clone();
                async move {
                    pending_block_opt.and_then(|pending_block| {
                        Self::filter_and_enrich_flashblock_static(&pending_block, &criteria)
                    })
                }
            });
        let pinned_stream = Box::pin(flashblocks_stream);

        tokio::spawn(async move {
            let _ = pipe_from_stream(sink, pinned_stream).await;
        });

        Ok(())
    }

    /// Filter and enrich a flashblock based on the provided criteria.
    ///
    /// Returns `None` if the block should be filtered out (e.g., no transactions match address
    /// filter).
    ///
    /// NOTE: This is a simplified implementation. For full receipt building with proper
    /// gas calculations and log indices, you would need to use `RpcConvert` trait methods.
    fn filter_and_enrich_flashblock_static(
        pending_block: &PendingFlashBlock<N>,
        criteria: &StreamCriteria,
    ) -> Option<EnrichedFlashblock<N::BlockHeader>> {
        use alloy_consensus::{transaction::TxHashRef, BlockHeader, TxReceipt};
        use reth_primitives_traits::SignedTransaction;

        // Extract header if requested
        let header = if criteria.new_heads {
            Some(extract_header_from_pending_block(pending_block).ok()?)
        } else {
            None
        };

        // Get block and receipts
        let block = pending_block.block();
        let receipts = pending_block.receipts.as_ref();

        // Filter and enrich transactions using transactions_with_sender
        let transactions: Vec<EnrichedTransaction> = block
            .transactions_with_sender()
            .enumerate()
            .filter_map(|(idx, (sender, tx))| {
                // Apply address filtering if specified
                if criteria.has_address_filter() {
                    let matches_filter = Self::transaction_matches_addresses_simple(
                        *sender,
                        tx,
                        receipts.get(idx),
                        &criteria.subscribed_addresses,
                    );
                    if !matches_filter {
                        return None;
                    }
                }

                let receipt = receipts.get(idx)?;
                let tx_hash = *tx.tx_hash();

                // Build TxData if requested
                let tx_data = if criteria.transaction_extra_info {
                    Some(Self::build_tx_data_json(*sender, tx, block))
                } else {
                    None
                };

                // Build Receipt if requested
                let receipt_json = if criteria.transaction_receipt {
                    Some(Self::build_receipt_json(tx_hash, receipt, block, idx))
                } else {
                    None
                };

                // Build enriched transaction
                Some(EnrichedTransaction { tx_hash, tx_data, receipt: receipt_json })
            })
            .collect();

        // If address filtering is enabled but no transactions matched, skip this block
        if criteria.has_address_filter() && transactions.is_empty() {
            return None;
        }

        Some(EnrichedFlashblock { header, transactions })
    }

    /// Check if a transaction matches any of the subscribed addresses (simplified version).
    fn transaction_matches_addresses_simple(
        sender: Address,
        tx: &N::SignedTx,
        receipt: Option<&N::Receipt>,
        addresses: &[Address],
    ) -> bool {
        use alloy_consensus::{Transaction, TxReceipt};
        use reth_primitives_traits::SignedTransaction;

        // Check sender
        if addresses.contains(&sender) {
            return true;
        }

        // Check recipient
        if let Some(to) = tx.to() {
            if addresses.contains(&to) {
                return true;
            }
        }

        // Check log addresses
        if let Some(receipt) = receipt {
            for log in receipt.logs() {
                if addresses.contains(&log.address) {
                    return true;
                }
            }
        }

        false
    }

    /// Build TxData JSON matching the user's specified format
    fn build_tx_data_json(
        sender: Address,
        tx: &N::SignedTx,
        block: &reth_primitives_traits::RecoveredBlock<N::Block>,
    ) -> serde_json::Value {
        use alloy_consensus::{transaction::TxHashRef, Transaction};
        use alloy_eips::eip2718::Typed2718;
        use alloy_primitives::hex;
        use serde_json::json;

        let tx_hash = *tx.tx_hash();
        let to = tx.to();
        let chain_id = tx.chain_id();
        let tx_type = tx.ty();

        // Build the JSON object matching the user's specified format
        let max_fee_per_gas = tx.max_fee_per_gas();
        let max_fee_per_gas_opt =
            if max_fee_per_gas > 0 { Some(format!("0x{:x}", max_fee_per_gas)) } else { None };

        json!({
            "type": format!("0x{:x}", tx_type),
            "nonce": format!("0x{:x}", tx.nonce()),
            "gasPrice": tx.gas_price().map(|gp| format!("0x{:x}", gp)),
            "maxFeePerGas": max_fee_per_gas_opt,
            "maxPriorityFeePerGas": tx.max_priority_fee_per_gas().map(|fee| format!("0x{:x}", fee)),
            "gas": format!("0x{:x}", tx.gas_limit()),
            "value": format!("0x{:x}", tx.value()),
            "input": format!("0x{}", hex::encode(tx.input())),
            "from": format!("{:?}", sender),
            "to": to.map(|addr| format!("{:?}", addr)),
            "chainId": chain_id.map(|id| format!("0x{:x}", id)),
            "hash": format!("{:?}", tx_hash),
        })
    }

    /// Build Receipt JSON matching the user's specified format
    fn build_receipt_json(
        tx_hash: alloy_primitives::TxHash,
        receipt: &N::Receipt,
        block: &reth_primitives_traits::RecoveredBlock<N::Block>,
        index: usize,
    ) -> serde_json::Value {
        use alloy_consensus::{BlockHeader, TxReceipt};
        use alloy_primitives::hex;
        use reth_primitives_traits::Block;
        use serde_json::json;

        let sealed_block = block.sealed_block();
        let block_hash = sealed_block.hash();
        let block_number = sealed_block.header().number();

        // Build logs array
        let logs: Vec<serde_json::Value> = receipt
            .logs()
            .iter()
            .enumerate()
            .map(|(log_idx, log)| {
                json!({
                    "address": format!("{:?}", log.address),
                    "topics": log.topics().iter().map(|t| format!("{:?}", t)).collect::<Vec<_>>(),
                    "data": format!("0x{}", hex::encode(&log.data.data)),
                    "blockNumber": format!("0x{:x}", block_number),
                    "blockHash": format!("{:?}", block_hash),
                    "transactionHash": format!("{:?}", tx_hash),
                    "transactionIndex": format!("0x{:x}", index),
                    "logIndex": format!("0x{:x}", log_idx),
                    "removed": false,
                })
            })
            .collect();

        // Build the receipt JSON object
        json!({
            "root": "0x",
            "status": format!("0x{:x}", if receipt.status() { 1 } else { 0 }),
            "cumulativeGasUsed": format!("0x{:x}", receipt.cumulative_gas_used()),
            "logsBloom": format!("0x{}", hex::encode(receipt.bloom().as_slice())),
            "logs": logs,
            "transactionHash": format!("{:?}", tx_hash),
            "contractAddress": "0x0000000000000000000000000000000000000000",
            "gasUsed": format!("0x{:x}", receipt.cumulative_gas_used()),
            "blockHash": format!("{:?}", block_hash),
            "blockNumber": format!("0x{:x}", block_number),
            "transactionIndex": format!("0x{:x}", index),
        })
    }
}

/// Pipes all stream items to the subscription sink.
///
/// This is a reusable helper function similar to the one in `reth_rpc::eth::pubsub`.
async fn pipe_from_stream<T, St>(
    sink: SubscriptionSink,
    stream: Pin<Box<St>>,
) -> Result<(), ErrorObject<'static>>
where
    St: Stream<Item = T> + ?Sized,
    T: Serialize,
{
    let mut stream = stream;
    loop {
        tokio::select! {
            _ = sink.closed() => {
                // connection dropped
                break Ok(())
            }
            maybe_item = StreamExt::next(&mut stream) => {
                let item = match maybe_item {
                    Some(item) => item,
                    None => {
                        // stream ended
                        break Ok(())
                    }
                };
                let msg = SubscriptionMessage::new(
                    sink.method_name(),
                    sink.subscription_id(),
                    &item,
                )
                .map_err(|e| internal_rpc_err(format!("Failed to serialize item: {e}")))?;

                if sink.send(msg).await.is_err() {
                    break Ok(());
                }
            }
        }
    }
}

/// Extract Header from PendingFlashBlock
///
/// Constructs an Ethereum RPC `Header` from a pending flashblock by extracting
/// the header from the executed block.
fn extract_header_from_pending_block<N: reth_primitives_traits::NodePrimitives>(
    pending_block: &PendingFlashBlock<N>,
) -> Result<Header<N::BlockHeader>, ErrorObject<'static>> {
    let block = pending_block.block();
    let sealed_header = block.clone_sealed_header();

    // Convert to RPC Header format
    // Note: We don't have block size, so we pass None
    Ok(Header::from_consensus(sealed_header.into(), None, None))
}
