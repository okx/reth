//! Contains RPC handler implementations for fee history.

use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_api::{
    helpers::{EthFees, LoadFee, XLayerFees},
    FromEvmError, RpcNodeCore,
};
use reth_rpc_eth_types::{EthApiError, FeeHistoryCache, GasPriceOracle};
use reth_storage_api::ProviderHeader;
use std::sync::Arc;

use crate::EthApi;

impl<N, Rpc> EthFees for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

impl<N, Rpc> LoadFee for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.inner.fee_history_cache()
    }
}

impl<N, Rpc> XLayerFees for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn set_pricer(&self, _pricer: Arc<dyn reth_rpc_eth_api::helpers::pricer::L2GasPricer>) {
        // No-op for ethereum node, XLayer functionality is not needed
    }

    fn get_pricer(&self) -> Option<Arc<dyn reth_rpc_eth_api::helpers::pricer::L2GasPricer>> {
        // No-op for ethereum node, XLayer functionality is not needed
        None
    }
}
