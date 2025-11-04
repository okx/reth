//! XLayer-specific extensions for OpEthApi

use super::OpEthApi;
use reth_rpc_eth_api::RpcNodeCore;
use reth_rpc_eth_api::RpcConvert;
use reth_rpc_eth_types::LegacyRpcClient;
use std::sync::Arc;

/// XLayer: Implement LegacyRpc trait for OpEthApi to enable legacy RPC routing
impl<N, Rpc> reth_rpc_eth_api::helpers::LegacyRpc for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    fn legacy_rpc_client(&self) -> Option<&Arc<LegacyRpcClient>> {
        self.inner.eth_api.legacy_rpc_client()
    }
}
