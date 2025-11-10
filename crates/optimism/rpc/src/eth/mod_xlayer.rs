//! XLayer-specific fee implementation for Optimism Ethereum API.

use crate::{OpEthApi, OpEthApiError, SequencerClient};
use alloy_json_rpc::{RpcRecv, RpcSend};
use reth_rpc_eth_api::{
    helpers::{pricer::L2GasPricer, XLayerFees},
    FromEvmError, RpcConvert, RpcNodeCore,
};
use std::{future::Future, sync::Arc};
use tracing::debug;

impl<N, Rpc> XLayerFees for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError>,
{
    type SequencerClient = SequencerClient;

    fn set_pricer(&self, pricer: Arc<dyn L2GasPricer>) {
        *self.inner.pricer.write() = Some(pricer);
    }

    fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>> {
        self.inner.pricer.read().clone()
    }

    fn sequencer_client(&self) -> Option<&Self::SequencerClient> {
        self.inner.sequencer_client()
    }

    /// Forwards a generic RPC request to the sequencer.
    ///
    /// Returns `Ok(Some(result))` if the request was successfully forwarded,
    /// `Ok(None)` if no sequencer is configured or forward failed.
    fn forward_to_sequencer<Params, Resp>(
        &self,
        method: &str,
        params: Params,
    ) -> impl Future<Output = Result<Option<Resp>, Self::Error>> + Send
    where
        Params: RpcSend,
        Resp: RpcRecv,
    {
        let sequencer = self.sequencer_client().cloned();
        let method = method.to_string();

        async move {
            let Some(sequencer) = sequencer else {
                return Ok(None);
            };

            debug!(target: "rpc::eth::xlayer", method = %method, "Forwarding RPC request to sequencer");

            match sequencer.request::<Params, Resp>(&method, params).await {
                Ok(result) => {
                    debug!(target: "rpc::eth::xlayer", method = %method, "Successfully received response from sequencer");
                    Ok(Some(result))
                }
                Err(_err) => {
                    debug!(target: "rpc::eth::xlayer", method = %method, "Failed to forward request to sequencer, will fall back to local");
                    // Return Ok(None) to indicate fallback to local logic
                    Ok(None)
                }
            }
        }
    }
}

