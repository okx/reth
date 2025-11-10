//! XLayer-specific fee implementation for Ethereum API.

use crate::EthApi;
use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_api::{
    helpers::{pricer::L2GasPricer, XLayerFees},
    FromEvmError, RpcNodeCore,
};
use reth_rpc_eth_types::EthApiError;
use std::sync::Arc;

impl<N, Rpc> XLayerFees for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn set_pricer(&self, _pricer: Arc<dyn L2GasPricer>) {
        // No-op for ethereum node, XLayer functionality is not needed
    }

    fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>> {
        // No-op for ethereum node, XLayer functionality is not needed
        None
    }
}

