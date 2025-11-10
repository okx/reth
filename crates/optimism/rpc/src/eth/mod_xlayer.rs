//! XLayer-specific fee implementation for Optimism Ethereum API.

use crate::{OpEthApi, OpEthApiError};
use reth_rpc_eth_api::{
    helpers::{pricer::L2GasPricer, XLayerFees},
    FromEvmError, RpcConvert, RpcNodeCore,
};
use std::sync::Arc;

impl<N, Rpc> XLayerFees for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError>,
{
    fn set_pricer(&self, pricer: Arc<dyn L2GasPricer>) {
        *self.inner.pricer.write() = Some(pricer);
    }

    fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>> {
        self.inner.pricer.read().clone()
    }
}

