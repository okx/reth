use crate::EngineApiError;
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use async_trait::async_trait;
use jsonrpsee_core::RpcResult;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::SealedBlock;
use reth_rpc_api::{RethEngineApiServer, RethNewPayloadInput, RethPayloadStatus};
use tracing::trace;

/// Standalone implementation of the `reth_` engine API namespace.
///
/// Provides the `reth_newPayload` endpoint that accepts either `ExecutionData` directly or an
/// RLP-encoded block, optionally waiting for persistence, execution cache, and sparse trie locks
/// before processing, and returns timing breakdowns with server-measured execution latency.
#[derive(Debug)]
pub struct RethEngineApi<Payload: PayloadTypes> {
    beacon_engine_handle: ConsensusEngineHandle<Payload>,
}

impl<Payload: PayloadTypes> RethEngineApi<Payload> {
    /// Creates a new [`RethEngineApi`].
    pub const fn new(beacon_engine_handle: ConsensusEngineHandle<Payload>) -> Self {
        Self { beacon_engine_handle }
    }
}

#[async_trait]
impl<Payload: PayloadTypes> RethEngineApiServer<Payload::ExecutionData> for RethEngineApi<Payload> {
    async fn reth_new_payload(
        &self,
        _input: RethNewPayloadInput<Payload::ExecutionData>,
        _wait_for_persistence: Option<bool>,
        _wait_for_caches: Option<bool>,
    ) -> RpcResult<RethPayloadStatus> {
        // Stubbed during the upstream merge — the underlying
        // BeaconEngineMessage::RethNewPayload variant was removed and is not yet ported.
        Err(EngineApiError::Internal(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "reth_newPayload is not implemented in this build",
        )))
        .into())
    }

    async fn reth_forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving reth_forkchoiceUpdated");
        self.beacon_engine_handle
            .fork_choice_updated(
                forkchoice_state,
                None,
                reth_engine_primitives::EngineApiMessageVersion::default(),
            )
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }
}
