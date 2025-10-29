//! Legacy RPC routing utilities
//!
//! Provides simple helpers to minimize code duplication when adding legacy routing logic.

use alloy_eips::BlockId;
use alloy_rpc_types_eth::BlockNumberOrTag;
use jsonrpsee::types::ErrorObjectOwned;
use serde::{Deserialize, Serialize};

/// Check if a block number should be routed to legacy RPC
#[inline]
pub fn should_route_to_legacy(
    legacy_client: Option<&std::sync::Arc<reth_rpc_eth_types::LegacyRpcClient>>,
    number: BlockNumberOrTag,
) -> bool {
    if let Some(client) = legacy_client {
        if let BlockNumberOrTag::Number(n) = number {
            return n < client.cutoff_block();
        }
    }
    false
}

/// Check if a BlockId should be routed to legacy RPC based on cutoff_block
#[inline]
pub fn should_route_block_id_to_legacy(
    legacy_client: Option<&std::sync::Arc<reth_rpc_eth_types::LegacyRpcClient>>,
    block_id: Option<BlockId>,
) -> bool {
    if let Some(client) = legacy_client {
        if let Some(BlockId::Number(number)) = block_id {
            return should_route_to_legacy(Some(client), number);
        }
    }
    false
}

/// Convert any value through serde JSON (for type system compatibility)
///
/// This is used to convert between `alloy_rpc_types_eth::Transaction` and `RpcTransaction<T::NetworkTypes>`
#[inline]
pub fn convert_via_serde<T, U>(value: T) -> Result<U, ErrorObjectOwned>
where
    T: Serialize,
    U: for<'de> Deserialize<'de>,
{
    let json = serde_json::to_value(value)
        .map_err(|e| internal_rpc_err(format!("Serialization error: {}", e)))?;
    serde_json::from_value(json)
        .map_err(|e| internal_rpc_err(format!("Deserialization error: {}", e)))
}

/// Convert Option<T> to Option<U> through serde
#[inline]
pub fn convert_option_via_serde<T, U>(value: Option<T>) -> Result<Option<U>, ErrorObjectOwned>
where
    T: Serialize,
    U: for<'de> Deserialize<'de>,
{
    match value {
        Some(v) => Ok(Some(convert_via_serde(v)?)),
        None => Ok(None),
    }
}

/// Helper to convert any error to internal RPC error
#[inline]
pub fn internal_rpc_err<E: std::fmt::Display>(e: E) -> ErrorObjectOwned {
    jsonrpsee::types::ErrorObjectOwned::owned(
        jsonrpsee::types::ErrorCode::InternalError.code(),
        e.to_string(),
        None::<()>,
    )
}

/// Helper to convert Box<dyn Error> to RPC error
#[inline]
pub fn boxed_err_to_rpc(e: Box<dyn std::error::Error + Send + Sync>) -> ErrorObjectOwned {
    internal_rpc_err(e.to_string())
}
