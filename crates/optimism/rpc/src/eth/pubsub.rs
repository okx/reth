use alloy_consensus::{transaction::TxHashRef, BlockHeader, Transaction, TxReceipt};
use alloy_json_rpc::RpcObject;
use alloy_primitives::Address;
use alloy_rpc_types_eth::{
    pubsub::{Params as AlloyParams, SubscriptionKind as AlloySubscriptionKind},
    Header, TransactionInfo,
};
use futures::StreamExt;
use jsonrpsee::{
    proc_macros::rpc, server::SubscriptionMessage, types::ErrorObject, PendingSubscriptionSink,
    SubscriptionSink,
};
use reth_optimism_flashblocks::{PendingBlockRx, PendingFlashBlock};
use reth_primitives_traits::{Recovered, TransactionMeta};
use reth_rpc::eth::pubsub::EthPubSub;
use reth_rpc_convert::{transaction::ConvertReceiptInput, RpcConvert};
use reth_rpc_eth_api::pubsub::EthPubSubApiServer;
use reth_rpc_server_types::result::internal_rpc_err;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::{wrappers::WatchStream, Stream};
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Subscription kind.
pub enum OpSubscriptionKind {
    /// Standard Ethereum subscription.
    Standard(AlloySubscriptionKind),
    /// Flashblocks subscription.
    Flashblocks,
}

impl<'de> Deserialize<'de> for OpSubscriptionKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        match s.as_str() {
            "flashblocks" => Ok(OpSubscriptionKind::Flashblocks),
            "newHeads" => Ok(OpSubscriptionKind::Standard(AlloySubscriptionKind::NewHeads)),
            "logs" => Ok(OpSubscriptionKind::Standard(AlloySubscriptionKind::Logs)),
            "newPendingTransactions" => {
                Ok(OpSubscriptionKind::Standard(AlloySubscriptionKind::NewPendingTransactions))
            }
            "syncing" => Ok(OpSubscriptionKind::Standard(AlloySubscriptionKind::Syncing)),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["flashblocks", "newHeads", "logs", "newPendingTransactions", "syncing"],
            )),
        }
    }
}

impl Serialize for OpSubscriptionKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            OpSubscriptionKind::Standard(kind) => {
                let s = match kind {
                    AlloySubscriptionKind::NewHeads => "newHeads",
                    AlloySubscriptionKind::Logs => "logs",
                    AlloySubscriptionKind::NewPendingTransactions => "newPendingTransactions",
                    AlloySubscriptionKind::Syncing => "syncing",
                };
                serializer.serialize_str(s)
            }
            OpSubscriptionKind::Flashblocks => serializer.serialize_str("flashblocks"),
        }
    }
}

impl OpSubscriptionKind {
    /// Returns the inner standard subscription kind, if any.
    pub const fn as_standard(&self) -> Option<&AlloySubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(kind),
            Self::Flashblocks => None,
        }
    }
}

impl From<AlloySubscriptionKind> for OpSubscriptionKind {
    fn from(kind: AlloySubscriptionKind) -> Self {
        Self::Standard(kind)
    }
}

impl From<OpSubscriptionKind> for AlloySubscriptionKind {
    fn from(kind: OpSubscriptionKind) -> Self {
        match kind {
            OpSubscriptionKind::Standard(alloy_kind) => alloy_kind,
            OpSubscriptionKind::Flashblocks => {
                unreachable!("Cannot convert Flashblocks to AlloySubscriptionKind")
            }
        }
    }
}

/// Extended params that wraps Alloy's `Params` and adds Optimism-specific variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    /// Standard Ethereum subscription params
    Standard(AlloyParams),
    /// Flashblocks stream filter
    FlashBlocksFilter(FlashBlocksFilter),
    /// No params
    None,
}

impl<'de> Deserialize<'de> for Params {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        if value.is_null() {
            return Ok(Params::None);
        }

        if let Ok(filter) = serde_json::from_value::<FlashBlocksFilter>(value.clone()) {
            return Ok(Params::FlashBlocksFilter(filter));
        }

        if let Ok(standard_params) = serde_json::from_value::<AlloyParams>(value.clone()) {
            return Ok(Params::Standard(standard_params));
        }

        Err(serde::de::Error::custom(
            "Invalid subscription parameters: must be valid FlashBlocksFilter or Filter",
        ))
    }
}

impl Serialize for Params {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Params::Standard(params) => params.serialize(serializer),
            Params::FlashBlocksFilter(filter) => filter.serialize(serializer),
            Params::None => serializer.serialize_none(),
        }
    }
}

impl Params {
    /// Returns the inner `FlashBlocksFilter` if this is a `FlashBlocksFilter` variant.
    pub fn as_flashblocks_filter(&self) -> Option<&FlashBlocksFilter> {
        match self {
            Params::FlashBlocksFilter(filter) => Some(filter),
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlashBlocksFilter {
    /// Include new block headers in the stream.
    #[serde(default)]
    pub header_info: bool,

    /// SubTxFilter
    #[serde(default)]
    pub sub_tx_filter: SubTxFilter,
}

/// Criteria for filtering and enriching transaction subscription data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubTxFilter {
    /// Include extra transaction information
    #[serde(default)]
    pub tx_info: bool,

    /// Include transaction receipts.
    #[serde(default)]
    pub tx_receipt: bool,

    /// Only include transactions involving these addresses
    #[serde(default)]
    pub subscribe_addresses: Vec<Address>,
}

impl SubTxFilter {
    /// Returns `true` if address filtering is enabled.
    pub fn has_address_filter(&self) -> bool {
        !self.subscribe_addresses.is_empty()
    }
}

/// Enriched flashblock data returned to subscribers based on FlashBlocksFilter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedFlashblock<H, Tx, R> {
    /// Block header (if `header_info` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Header<H>>,

    /// Filtered transactions with optional enrichment
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub transactions: Vec<EnrichedTransaction<Tx, R>>,
}

/// Transaction data with optional enrichment based on FlashBlocksFilter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedTransaction<Tx, R> {
    /// Transaction hash
    pub tx_hash: alloy_primitives::TxHash,

    /// Transaction data (if `tx_info` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_data: Option<Tx>,

    /// Transaction receipt (if `tx_receipt` is true in criteria)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<R>,
}

impl FlashBlocksFilter {
    /// Returns `true` if address filtering is enabled.
    pub fn has_sub_tx_filter(&self) -> bool {
        self.sub_tx_filter.has_address_filter()
    }
}

/// Flashblocks pubsub RPC interface.
#[rpc(server, namespace = "eth")]
pub trait FlashblocksPubSubApi<T: RpcObject> {
    /// Create an ethereum subscription for the given params
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = alloy_rpc_types::pubsub::SubscriptionResult
    )]
    async fn subscribe(
        &self,
        kind: OpSubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult;
}

/// Optimism-specific Ethereum pubsub handler that extends standard subscriptions with flashblocks
/// support.
pub struct OpEthPubSub<Eth, N: reth_primitives_traits::NodePrimitives> {
    /// Standard eth pubsub handler
    eth_pubsub: EthPubSub<Eth>,
    /// Pending block receiver from flashblocks, if available
    pending_block_rx: Option<PendingBlockRx<N>>,
    /// Direct reference to eth API for RPC conversion
    eth_api: Eth,
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> Clone for OpEthPubSub<Eth, N>
where
    Eth: Clone,
{
    fn clone(&self) -> Self {
        Self {
            eth_pubsub: self.eth_pubsub.clone(),
            pending_block_rx: self.pending_block_rx.clone(),
            eth_api: self.eth_api.clone(),
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
    pub fn new(
        eth_pubsub: EthPubSub<Eth>,
        pending_block_rx: Option<PendingBlockRx<N>>,
        eth_api: Eth,
    ) -> Self {
        Self { eth_pubsub, pending_block_rx, eth_api }
    }

    /// Returns a reference to the wrapped `EthPubSub`.
    pub fn eth_pubsub(&self) -> &EthPubSub<Eth> {
        &self.eth_pubsub
    }

    /// Converts this `OpEthPubSub` into an RPC module.
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
            Primitives = N,
            Provider: reth_storage_api::BlockNumReader
                          + reth_chain_state::CanonStateSubscriptions<Primitives = N>,
            Pool: reth_transaction_pool::TransactionPool,
        > + reth_rpc_eth_api::EthApiTypes<RpcConvert: reth_rpc_convert::RpcConvert<Primitives = N>>
        + 'static,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: OpSubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        info!("XXX line 163: {:?}, params: {:?}", kind, params);

        // Extract FlashBlocksFilter from params if present
        let filter =
            params.as_ref().and_then(|p| p.as_flashblocks_filter()).cloned().unwrap_or_default();
        info!("XXX using filter: {:?}", filter);

        if (filter.sub_tx_filter.tx_info || filter.sub_tx_filter.tx_receipt) &&
            filter.sub_tx_filter.subscribe_addresses.is_empty()
        {
            let err = internal_rpc_err(
                "subscribeAddresses is required when txInfo or txReceipt is enabled. Provide at least one address to monitor.",
            );
            pending.accept().await?;
            return Err(jsonrpsee::core::SubscriptionError::from(err));
        }

        match kind {
            OpSubscriptionKind::Flashblocks => {
                // Handle flashblocks subscription
                if let Some(pending_block_rx) = &self.pending_block_rx {
                    info!(
                        "XXX flashblocks subscription with pending_block_rx available, filter: {:?}",
                        filter
                    );
                    return self.filter_flashblocks_stream(pending, pending_block_rx, &filter).await;
                } else {
                    let err = internal_rpc_err("Flashblocks are not available on this node");
                    pending.accept().await?;
                    return Err(jsonrpsee::core::SubscriptionError::from(err));
                }
            }
            OpSubscriptionKind::Standard(alloy_kind) => {
                let standard_params = params.and_then(|p| p.as_standard().cloned());

                self.eth_pubsub.subscribe(pending, alloy_kind, standard_params).await?;
                Ok(())
            }
        }
    }
}

impl<Eth, N: reth_primitives_traits::NodePrimitives> OpEthPubSub<Eth, N>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<
            Primitives = N,
            Provider: reth_storage_api::BlockNumReader
                          + reth_chain_state::CanonStateSubscriptions<Primitives = N>,
            Pool: reth_transaction_pool::TransactionPool,
        > + reth_rpc_eth_api::EthApiTypes<RpcConvert: reth_rpc_convert::RpcConvert<Primitives = N>>
        + 'static,
{
    async fn filter_flashblocks_stream(
        &self,
        pending: PendingSubscriptionSink,
        pending_block_rx: &PendingBlockRx<N>,
        filter: &FlashBlocksFilter,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let pending_block_rx = pending_block_rx.clone();
        let filter = filter.clone();
        let api = self.eth_api.clone();

        let flashblocks_stream =
            WatchStream::new(pending_block_rx).filter_map(move |pending_block_opt| {
                let filter = filter.clone();
                let api = api.clone();
                async move {
                    pending_block_opt.and_then(|pending_block| {
                        Self::filter_and_enrich_flashblock(&pending_block, &filter, &api)
                    })
                }
            });
        let pinned_stream = Box::pin(flashblocks_stream);

        tokio::spawn(async move {
            let _ = pipe_from_stream(sink, pinned_stream).await;
        });

        Ok(())
    }

    /// Filter and enrich a flashblock based on the provided criteria using RpcConvert.
    fn filter_and_enrich_flashblock(
        pending_block: &PendingFlashBlock<N>,
        filter: &FlashBlocksFilter,
        api: &Eth,
    ) -> Option<
        EnrichedFlashblock<
            N::BlockHeader,
            reth_rpc_eth_api::RpcTransaction<
                <Eth::RpcConvert as reth_rpc_convert::RpcConvert>::Network,
            >,
            reth_rpc_eth_api::RpcReceipt<
                <Eth::RpcConvert as reth_rpc_convert::RpcConvert>::Network,
            >,
        >,
    > {
        // Extract header if requested
        let header = if filter.header_info {
            Some(extract_header_from_pending_block(pending_block).ok()?)
        } else {
            None
        };

        let block = pending_block.block();
        let receipts = pending_block.receipts.as_ref();
        let sealed_block = block.sealed_block();
        let rpc_convert = api.tx_resp_builder();

        // Filter and enrich transactions using transactions_with_sender
        let transactions: Vec<
            EnrichedTransaction<
                reth_rpc_eth_api::RpcTransaction<
                    <Eth::RpcConvert as reth_rpc_convert::RpcConvert>::Network,
                >,
                reth_rpc_eth_api::RpcReceipt<
                    <Eth::RpcConvert as reth_rpc_convert::RpcConvert>::Network,
                >,
            >,
        > = block
            .transactions_with_sender()
            .enumerate()
            .filter_map(|(idx, (sender, tx))| {
                if filter.has_sub_tx_filter() {
                    let matches_filter = Self::is_address_in_transaction(
                        *sender,
                        tx,
                        receipts.get(idx),
                        &filter.sub_tx_filter.subscribe_addresses,
                    );
                    if !matches_filter {
                        return None;
                    }
                }

                let receipt = receipts.get(idx)?;
                let tx_hash = *tx.tx_hash();

                let tx_data = if filter.sub_tx_filter.tx_info {
                    let recovered =
                        reth_primitives_traits::Recovered::new_unchecked(tx.clone(), *sender);

                    let rpc_tx = rpc_convert
                        .fill(
                            recovered,
                            TransactionInfo {
                                hash: Some(tx_hash),
                                index: Some(idx as u64),
                                block_hash: Some(sealed_block.hash()),
                                block_number: Some(sealed_block.header().number()),
                                base_fee: sealed_block.header().base_fee_per_gas(),
                            },
                        )
                        .ok()?;

                    Some(rpc_tx)
                } else {
                    None
                };

                let receipt_json = if filter.sub_tx_filter.tx_receipt {
                    let gas_used = receipt.cumulative_gas_used();

                    let next_log_index =
                        receipts.iter().take(idx).map(|r| r.logs().len()).sum::<usize>();

                    let receipt_input = ConvertReceiptInput {
                        receipt: receipt.clone(),
                        tx: Recovered::new_unchecked(tx, *sender),
                        gas_used,
                        next_log_index,
                        meta: TransactionMeta {
                            tx_hash,
                            index: idx as u64,
                            block_hash: sealed_block.hash(),
                            block_number: sealed_block.header().number(),
                            base_fee: sealed_block.header().base_fee_per_gas(),
                            excess_blob_gas: sealed_block.header().excess_blob_gas(),
                            timestamp: sealed_block.header().timestamp(),
                        },
                    };

                    let rpc_receipts = rpc_convert
                        .convert_receipts_with_block(vec![receipt_input], &sealed_block)
                        .ok()?;

                    rpc_receipts.first().cloned()
                } else {
                    None
                };

                Some(EnrichedTransaction { tx_hash, tx_data, receipt: receipt_json })
            })
            .collect();

        if filter.sub_tx_filter.has_address_filter() && transactions.is_empty() {
            return None;
        }

        Some(EnrichedFlashblock { header, transactions })
    }

    fn is_address_in_transaction(
        sender: Address,
        tx: &N::SignedTx,
        receipt: Option<&N::Receipt>,
        addresses: &[Address],
    ) -> bool {
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
}

/// Pipes all stream items to the subscription sink.
///
/// Takes `Pin<Box<St>>` because streams created with async closures in `filter_map()`
/// are !Unpin. This is necessary when using `WatchStream` with async transformations.
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
fn extract_header_from_pending_block<N: reth_primitives_traits::NodePrimitives>(
    pending_block: &PendingFlashBlock<N>,
) -> Result<Header<N::BlockHeader>, ErrorObject<'static>> {
    let block = pending_block.block();
    let sealed_header = block.clone_sealed_header();

    Ok(Header::from_consensus(sealed_header.into(), None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_flashblocks_filter_serialization() {
        let filter = FlashBlocksFilter {
            header_info: true,
            sub_tx_filter: SubTxFilter {
                tx_info: true,
                tx_receipt: false,
                subscribe_addresses: vec![address!("0x1234567890123456789012345678901234567890")],
            },
        };

        let json = serde_json::to_string(&filter).unwrap();
        println!("Serialized: {}", json);

        let deserialized: FlashBlocksFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter, deserialized);
    }

    #[test]
    fn test_params_deserialization() {
        let json = r#"{"headerInfo": true, "subTxFilter": {"txInfo": true, "txReceipt": false, "subscribeAddresses": ["0x1234567890123456789012345678901234567890"]}}"#;

        let params: Params = serde_json::from_str(json).unwrap();

        match params {
            Params::FlashBlocksFilter(filter) => {
                assert!(filter.header_info);
                assert!(filter.sub_tx_filter.tx_info);
                assert!(!filter.sub_tx_filter.tx_receipt);
                assert_eq!(filter.sub_tx_filter.subscribe_addresses.len(), 1);
            }
            _ => panic!("Expected FlashBlocksFilter variant"),
        }
    }

    #[test]
    fn test_params_with_empty_sub_tx_filter() {
        let json = r#"{"headerInfo": true}"#;

        let params: Params = serde_json::from_str(json).unwrap();

        match params {
            Params::FlashBlocksFilter(filter) => {
                assert!(filter.header_info);
                assert!(!filter.sub_tx_filter.tx_info);
                assert!(!filter.sub_tx_filter.tx_receipt);
                assert_eq!(filter.sub_tx_filter.subscribe_addresses.len(), 0);
            }
            _ => panic!("Expected FlashBlocksFilter variant"),
        }
    }

    #[test]
    fn test_params_none_from_null() {
        let json = r#"null"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(matches!(params, Params::None));
    }

    #[test]
    fn test_option_params_none() {
        let json = r#"null"#;
        let params: Option<Params> = serde_json::from_str(json).unwrap();
        assert!(params.is_none());
    }
}
