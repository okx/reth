use alloy_consensus::{Header as ConsensusHeader, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::merge::BEACON_NONCE;
use alloy_json_rpc::RpcObject;
use alloy_primitives::{FixedBytes, U256};
use alloy_rpc_types_eth::{
    pubsub::{Params, SubscriptionKind},
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
use serde::Serialize;
use std::pin::Pin;
use tokio_stream::{wrappers::WatchStream, Stream};
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
    EthPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>> for OpEthPubSub<Eth, N>
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
            if let Some(pending_block_rx) = &self.pending_block_rx {
                info!("XXX line 96: {:?}", kind);
                return self
                    .subscribe_pending_blocks_as_headers(pending, pending_block_rx, params)
                    .await;
            }
        }

        // Fall back to standard handler for all other cases
        self.eth_pubsub.subscribe(pending, kind, params).await
    }
}

#[async_trait::async_trait]
impl<Eth, N: reth_primitives_traits::NodePrimitives>
    FlashblocksPubSubApiServer<reth_rpc_eth_api::RpcTransaction<Eth::NetworkTypes>>
    for OpEthPubSub<Eth, N>
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
            info!("XXX line 166: {:?}", kind);
            if let Some(pending_block_rx) = &self.pending_block_rx {
                info!("XXX line 167: pending_block_rx available");
                return self
                    .subscribe_pending_blocks_as_headers(pending, pending_block_rx, params)
                    .await;
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
        _params: Option<alloy_rpc_types_eth::pubsub::Params>,
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

/// Extract Header from FlashBlock
///
/// Constructs an Ethereum RPC `Header` from a flashblock payload by combining
/// the immutable `base` fields with the mutable `diff` fields.
///
/// Returns `Header<alloy_consensus::Header>` which can be converted to other header types
/// if needed by the caller.
fn extract_header_from_flashblock(
    flashblock: &FlashBlock,
) -> Result<Header<ConsensusHeader>, ErrorObject<'static>> {
    // Get base fields (immutable, only present in first flashblock)
    info!("XXX line 275: {:?}", flashblock.payload_id);
    let base = flashblock.base.as_ref().ok_or_else(|| {
        internal_rpc_err("Flashblock missing base fields required for header construction")
    })?;

    // Get diff fields (mutable, updated with each flashblock)
    let diff = &flashblock.diff;

    // // Compute transactions root from the transactions list
    // let transactions_root = if diff.transactions.is_empty() {
    //     // Empty transactions list has a specific root
    //     proofs::calculate_transaction_root(&[])
    // } else {
    //     // Calculate root from transaction bytes
    //     // Note: diff.transactions is Vec<Bytes>, we need to decode them or compute root directly
    //     // For now, we'll use the receipts_root as a proxy if transactions_root isn't directly
    // available     // In a proper implementation, we'd decode transactions and compute the
    // root     // This is a limitation - we'd need the actual transaction objects to compute
    // the root correctly     // For now, we'll use a placeholder that should be computed from
    // the actual transactions     proofs::calculate_transaction_root(&[])
    // };

    info!("XXX line 301");
    // Construct consensus header fields
    // TODO: Properly compute transactions_root from diff.transactions (RLP-encoded bytes)
    // For now, using empty root as placeholder - this should be computed from actual transactions
    let consensus_header = ConsensusHeader {
        parent_hash: base.parent_hash,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: base.fee_recipient,
        state_root: diff.state_root,
        transactions_root: FixedBytes::default(),
        receipts_root: diff.receipts_root,
        withdrawals_root: Some(diff.withdrawals_root),
        logs_bloom: diff.logs_bloom,
        timestamp: base.timestamp,
        mix_hash: base.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: base.base_fee_per_gas.to::<u64>().into(),
        number: base.block_number,
        gas_limit: base.gas_limit,
        difficulty: U256::ZERO, // PoS chains have zero difficulty
        gas_used: diff.gas_used,
        extra_data: base.extra_data.clone(),
        parent_beacon_block_root: Some(base.parent_beacon_block_root),
        blob_gas_used: None,   // Not available in flashblock diff structure
        excess_blob_gas: None, // Not available in flashblock, would need to compute
        requests_hash: None,   // Not available in flashblock
    };
    info!("XXX line 327");

    // Seal the header with the block hash from diff
    // Use the known block hash from diff instead of computing it
    let sealed_header = SealedHeader::new(consensus_header, diff.block_hash);
    info!("XXX line 331: {:?}", sealed_header);
    // Convert to RPC Header format
    // Note: We don't have block size, so we pass None
    // The sealed header is converted with .into() to match the expected type for from_consensus
    Ok(Header::from_consensus(sealed_header.into(), None, None))
}
