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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use std::sync::Arc;
    use std::time::Duration;

    /// Helper to create a test legacy client
    fn create_test_client(cutoff_block: u64) -> Arc<reth_rpc_eth_types::LegacyRpcClient> {
        let config = reth_rpc_eth_types::LegacyRpcConfig::new(
            cutoff_block,
            "http://test:8545".to_string(),
            Duration::from_secs(30),
        );
        Arc::new(reth_rpc_eth_types::LegacyRpcClient::from_config(&config).unwrap())
    }

    // ========================================
    // Phase 1.1: 类型转换测试（最关键）⭐
    // ========================================

    #[test]
    fn test_convert_simple_types() {
        // Test primitive types
        let num: u64 = 42;
        let result: u64 = convert_via_serde(num).unwrap();
        assert_eq!(result, 42);

        let addr = Address::from([1u8; 20]);
        let result: Address = convert_via_serde(addr).unwrap();
        assert_eq!(result, addr);

        let hash = B256::from([2u8; 32]);
        let result: B256 = convert_via_serde(hash).unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn test_convert_u256() {
        let value = U256::from(1234567890u64);
        let result: U256 = convert_via_serde(value).unwrap();
        assert_eq!(result, value);

        // Test large numbers
        let large = U256::from_str_radix("ffffffffffffffffffffffffffffffff", 16).unwrap();
        let result: U256 = convert_via_serde(large).unwrap();
        assert_eq!(result, large);
    }

    #[test]
    fn test_convert_option_types() {
        // Test Some
        let value = Some(42u64);
        let result: Option<u64> = convert_option_via_serde(value).unwrap();
        assert_eq!(result, Some(42));

        // Test None
        let value: Option<u64> = None;
        let result: Option<u64> = convert_option_via_serde(value).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_convert_block_header_fields() {
        // Test converting common block fields
        use serde_json::json;

        let block_json = json!({
            "number": "0x1",
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "timestamp": "0x123456",
            "gasLimit": "0x1000000",
            "gasUsed": "0x5000",
        });

        // This tests the core serde conversion logic
        let result: serde_json::Value = convert_via_serde(block_json.clone()).unwrap();
        assert_eq!(result, block_json);
    }

    #[test]
    fn test_convert_transaction_fields() {
        use serde_json::json;

        let tx_json = json!({
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "from": "0x0000000000000000000000000000000000000001",
            "to": "0x0000000000000000000000000000000000000002",
            "value": "0x1000",
            "gas": "0x5208",
            "gasPrice": "0x3b9aca00",
            "nonce": "0x0",
        });

        let result: serde_json::Value = convert_via_serde(tx_json.clone()).unwrap();
        assert_eq!(result, tx_json);
    }

    #[test]
    fn test_convert_nested_structures() {
        use serde_json::json;

        // Test nested arrays and objects
        let nested = json!({
            "transactions": [
                {"hash": "0x01", "value": "0x100"},
                {"hash": "0x02", "value": "0x200"},
            ],
            "uncles": [],
        });

        let result: serde_json::Value = convert_via_serde(nested.clone()).unwrap();
        assert_eq!(result, nested);
    }

    #[test]
    fn test_serde_conversion_preserves_hex_encoding() {
        // This is critical - hex strings must be preserved correctly
        use serde_json::json;

        let data = json!({
            "blockNumber": "0xabc123",
            "blockHash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
        });

        let result: serde_json::Value = convert_via_serde(data.clone()).unwrap();
        assert_eq!(result["blockNumber"], "0xabc123");
        assert_eq!(
            result["blockHash"],
            "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        );
    }

    // ========================================
    // Phase 1.2: 路由逻辑测试
    // ========================================

    #[test]
    fn test_should_route_to_legacy_below_cutoff() {
        let client = create_test_client(1000000);

        // Below cutoff - should route to legacy
        assert!(should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(100)));
        assert!(should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(999999)));
    }

    #[test]
    fn test_should_route_to_legacy_at_and_above_cutoff() {
        let client = create_test_client(1000000);

        // At cutoff - should NOT route to legacy
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(1000000)));

        // Above cutoff - should NOT route to legacy
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(1000001)));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(2000000)));
    }

    #[test]
    fn test_should_route_to_legacy_special_tags() {
        let client = create_test_client(1000000);

        // Special tags should NOT route to legacy (handled locally)
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Latest));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Earliest));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Pending));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Finalized));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Safe));
    }

    #[test]
    fn test_should_route_to_legacy_no_client() {
        // No client - should never route to legacy
        assert!(!should_route_to_legacy(None, BlockNumberOrTag::Number(100)));
        assert!(!should_route_to_legacy(None, BlockNumberOrTag::Latest));
    }

    #[test]
    fn test_should_route_block_id_to_legacy() {
        let client = create_test_client(1000000);

        // BlockId::Number below cutoff
        let block_id = Some(BlockId::Number(BlockNumberOrTag::Number(100)));
        assert!(should_route_block_id_to_legacy(Some(&client), block_id));

        // BlockId::Number above cutoff
        let block_id = Some(BlockId::Number(BlockNumberOrTag::Number(1000001)));
        assert!(!should_route_block_id_to_legacy(Some(&client), block_id));

        // BlockId::Hash - should NOT route (can't determine block number from hash)
        let block_id = Some(BlockId::Hash(B256::from([1u8; 32]).into()));
        assert!(!should_route_block_id_to_legacy(Some(&client), block_id));

        // None - should NOT route
        assert!(!should_route_block_id_to_legacy(Some(&client), None));
    }

    #[test]
    fn test_edge_case_cutoff_at_zero() {
        let client = create_test_client(0);

        // Everything should be handled locally
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(0)));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(1)));
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(1000)));
    }

    #[test]
    fn test_edge_case_cutoff_at_max() {
        let client = create_test_client(u64::MAX);

        // Everything should route to legacy (except special tags)
        assert!(should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(0)));
        assert!(should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(1000000)));
        assert!(should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(u64::MAX - 1)));

        // But not u64::MAX itself
        assert!(!should_route_to_legacy(Some(&client), BlockNumberOrTag::Number(u64::MAX)));
    }

    // ========================================
    // 错误处理测试
    // ========================================

    #[test]
    fn test_internal_rpc_err_preserves_message() {
        let err = internal_rpc_err("Test error message");
        assert_eq!(err.code(), jsonrpsee::types::ErrorCode::InternalError.code());
        assert!(err.message().contains("Test error message"));
    }

    #[test]
    fn test_boxed_err_to_rpc() {
        let err: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, "IO error"));
        let rpc_err = boxed_err_to_rpc(err);
        assert!(rpc_err.message().contains("IO error"));
    }

    #[test]
    fn test_convert_error_on_incompatible_types() {
        use serde_json::json;

        // Try to convert string to number - should fail
        let value = json!("not a number");
        let result: Result<u64, _> = convert_via_serde(value);
        assert!(result.is_err());

        if let Err(err) = result {
            assert!(err.message().contains("Deserialization error"));
        }
    }
}
