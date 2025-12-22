use alloy_json_rpc::RpcObject;
use alloy_rpc_types_eth::{
    pubsub::{Params, SubscriptionKind},
    Header,
};
use futures::StreamExt;
use jsonrpsee::{
    proc_macros::rpc, server::SubscriptionMessage, types::ErrorObject, PendingSubscriptionSink,
    SubscriptionSink,
};
use reth_optimism_flashblocks::FlashBlock;
use reth_rpc::eth::pubsub::EthPubSub;
use reth_rpc_eth_api::pubsub::EthPubSubApiServer;
use reth_rpc_server_types::result::internal_rpc_err;
use serde::Serialize;
use std::{pin::Pin, sync::Arc};
use tokio_stream::{wrappers::BroadcastStream, Stream};
use tracing::info;

/// Flashblocks pubsub RPC interface.
///
/// This trait provides the same interface as `EthPubSubApi` but with relaxed trait bounds
/// to allow implementation for `OpEthPubSub` without requiring `SignedTx = PoolConsensusTx<Pool>`.
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
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult;
}

#[derive(Clone, Debug)]
pub struct OpEthPubSub<Eth> {
    /// Standard eth pubsub handler
    eth_pubsub: EthPubSub<Eth>,
    /// Flashblocks broadcast sender, if available
    flashblocks_tx: Option<tokio::sync::broadcast::Sender<Arc<FlashBlock>>>,
}

impl<Eth> OpEthPubSub<Eth> {
    /// Creates a new `OpEthPubSub` instance.
    pub fn new(
        eth_pubsub: EthPubSub<Eth>,
        flashblocks_tx: Option<tokio::sync::broadcast::Sender<Arc<FlashBlock>>>,
    ) -> Self {
        Self { eth_pubsub, flashblocks_tx }
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
        OpEthPubSub<Eth>:
            FlashblocksPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>,
    {
        <OpEthPubSub<Eth> as FlashblocksPubSubApiServer<
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
    ) -> Option<
        impl Stream<
            Item = Header<<Eth::Primitives as reth_primitives_traits::NodePrimitives>::BlockHeader>,
        >,
    >
    where
        Eth: reth_rpc_eth_api::RpcNodeCore,
    {
        self.flashblocks_tx.as_ref().map(|flashblocks_tx| {
            BroadcastStream::new(flashblocks_tx.subscribe()).filter_map(|result| async move {
                match result {
                    Ok(flashblock) => extract_header_from_flashblock::<
                        <Eth::Primitives as reth_primitives_traits::NodePrimitives>::BlockHeader,
                    >(&flashblock)
                    .ok(),
                    Err(_) => {
                        // BroadcastStream lagged, skip
                        None
                    }
                }
            })
        })
    }
}

#[async_trait::async_trait]
impl<Eth> EthPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>
    for OpEthPubSub<Eth>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<
            Provider: reth_storage_api::BlockNumReader + reth_chain_state::CanonStateSubscriptions,
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
        kind: alloy_rpc_types_eth::pubsub::SubscriptionKind,
        params: Option<alloy_rpc_types_eth::pubsub::Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        // Intercept newHeads subscription and use flashblocks if available
        info!("XXX subscribe: {:?}", kind);
        if matches!(kind, alloy_rpc_types_eth::pubsub::SubscriptionKind::NewHeads) {
            if let Some(flashblocks_tx) = &self.flashblocks_tx {
                info!("XXX line 96: {:?}", kind);
                return self.subscribe_flashblocks_as_headers(pending, flashblocks_tx, params).await;
            }
        }

        // Fall back to standard handler for all other cases
        self.eth_pubsub.subscribe(pending, kind, params).await
    }
}

#[async_trait::async_trait]
impl<Eth> FlashblocksPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>
    for OpEthPubSub<Eth>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<
            Provider: reth_storage_api::BlockNumReader + reth_chain_state::CanonStateSubscriptions,
            Pool: reth_transaction_pool::TransactionPool,
        > + reth_rpc_eth_api::EthApiTypes<
            RpcConvert: reth_rpc_eth_api::RpcConvert<
                Primitives: reth_primitives_traits::NodePrimitives,
            >,
        > + 'static,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        info!("XXX line 163: {:?}", kind);
        if matches!(kind, SubscriptionKind::NewHeads) {
            if let Some(flashblocks_tx) = &self.flashblocks_tx {
                return self.subscribe_flashblocks_as_headers(pending, flashblocks_tx, params).await;
            }
        }

        info!("XXX line 173");
        // TODO: Implement full fallback logic or ensure standard pubsub is merged after this
        let err = internal_rpc_err(
            "This subscription type should be handled by the standard pubsub. \
             FlashblocksPubSubApiServer only handles newHeads with flashblocks enabled.",
        );
        pending.accept().await?;

        Err(jsonrpsee::core::SubscriptionError::from(err))
    }
}

impl<Eth> OpEthPubSub<Eth>
where
    Eth: reth_rpc_eth_api::RpcNodeCore<Provider: reth_storage_api::BlockNumReader>
        + reth_rpc_eth_api::EthApiTypes<
            RpcConvert: reth_rpc_eth_api::RpcConvert<
                Primitives: reth_primitives_traits::NodePrimitives,
            >,
        >,
{
    /// Subscribe to flashblocks and convert them to Header format for newHeads
    async fn subscribe_flashblocks_as_headers(
        &self,
        pending: PendingSubscriptionSink,
        flashblocks_tx: &tokio::sync::broadcast::Sender<Arc<FlashBlock>>,
        _params: Option<alloy_rpc_types_eth::pubsub::Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let flashblocks_rx = flashblocks_tx.subscribe();
        info!("XXX line 202: {:?}", flashblocks_rx);
        // Convert flashblocks stream to headers stream, filtering out errors
        let headers_stream = BroadcastStream::new(flashblocks_rx).filter_map(|result| async move {
            match result {
                Ok(flashblock) => Some(flashblock),
                Err(_) => {
                    // BroadcastStream lagged, skip
                    None
                }
            }
        });
        info!("XXX line 213");
        let pinned_stream = Box::pin(headers_stream);

        tokio::spawn(async move {
            let _ = pipe_from_stream(sink, pinned_stream).await;
        });

        Ok(())
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

/// Extract Header from FlashBlock
fn extract_header_from_flashblock<BlockHeader>(
    _flashblock: &FlashBlock,
) -> Result<Header<BlockHeader>, ErrorObject<'static>>
where
    BlockHeader: alloy_consensus::BlockHeader,
{
    info!("XXX line 272: {:?}", _flashblock);
    Err(internal_rpc_err("Header extraction from flashblock not yet implemented"))
}
