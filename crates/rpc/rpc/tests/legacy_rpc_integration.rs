//! Integration tests for Legacy RPC routing
//!
//! These tests verify:
//! - Basic routing logic with mock legacy RPC
//! - Cross-boundary getLogs
//! - Hash-based fallback

use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    server::{ServerBuilder, ServerHandle},
    RpcModule,
};
use reth_rpc_eth_types::{CrossBoundaryFilterManager, LegacyRpcClient, LegacyRpcConfig};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{Block, BlockNumberOrTag, Filter, FilterBlockOption, Log, Transaction};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ========================================
// Mock Legacy RPC Server
// ========================================

/// Creates a mock legacy RPC server that responds to basic eth_ methods
async fn create_mock_legacy_server() -> (ServerHandle, SocketAddr) {
    let server = ServerBuilder::default()
        .build("127.0.0.1:0")
        .await
        .expect("Failed to build mock server");

    let addr = server.local_addr().expect("Failed to get local addr");

    let mut module = RpcModule::new(());

    // Mock eth_getBlockByNumber
    module
        .register_async_method("eth_getBlockByNumber", |params, _, _| async move {
            let (block_num_str, full): (String, bool) = params.parse()?;

            // Parse block number from hex
            let block_num = if block_num_str.starts_with("0x") {
                u64::from_str_radix(&block_num_str[2..], 16).unwrap_or(0)
            } else {
                block_num_str.parse().unwrap_or(0)
            };

            // Return a mock block
            let block = Block {
                header: alloy_rpc_types_eth::Header {
                    number: Some(block_num),
                    hash: Some(B256::from([1u8; 32])),
                    parent_hash: B256::from([0u8; 32]),
                    timestamp: 1234567890,
                    gas_limit: 10000000,
                    gas_used: 5000,
                    ..Default::default()
                },
                transactions: if full {
                    alloy_rpc_types_eth::BlockTransactions::Full(vec![])
                } else {
                    alloy_rpc_types_eth::BlockTransactions::Hashes(vec![])
                },
                uncles: vec![],
                ..Default::default()
            };

            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(Some(block))
        })
        .expect("Failed to register method");

    // Mock eth_getTransactionByHash
    module
        .register_async_method("eth_getTransactionByHash", |params, _, _| async move {
            let (hash,): (B256,) = params.parse()?;

            // Return a mock transaction
            let tx = alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::TxLegacy {
                    chain_id: Some(1),
                    nonce: 0,
                    gas_price: 1000000000,
                    gas_limit: 21000,
                    to: alloy_primitives::TxKind::Call(Address::from([2u8; 20])),
                    value: U256::from(1000),
                    input: Default::default(),
                }.into(),
                block_hash: Some(B256::from([1u8; 32])),
                block_number: Some(100),
                transaction_index: Some(0),
                effective_gas_price: Some(1000000000),
            };

            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(Some(tx))
        })
        .expect("Failed to register method");

    // Mock eth_getLogs
    module
        .register_async_method("eth_getLogs", |params, _, _| async move {
            let (filter,): (Filter,) = params.parse()?;

            // Parse block range
            let (from_block, to_block) = match &filter.block_option {
                FilterBlockOption::Range { from_block, to_block } => {
                    let from = match from_block {
                        Some(BlockNumberOrTag::Number(n)) => *n,
                        _ => 0,
                    };
                    let to = match to_block {
                        Some(BlockNumberOrTag::Number(n)) => *n,
                        _ => u64::MAX,
                    };
                    (from, to)
                }
                _ => (0, u64::MAX),
            };

            // Return mock logs for the range
            let mut logs = vec![];
            for block_num in from_block..=to_block.min(from_block + 2) {
                logs.push(Log {
                    inner: alloy_primitives::Log {
                        address: Address::from([3u8; 20]),
                        data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
                    },
                    block_hash: Some(B256::from([1u8; 32])),
                    block_number: Some(block_num),
                    block_timestamp: None,
                    transaction_hash: Some(B256::from([2u8; 32])),
                    transaction_index: Some(0),
                    log_index: Some(0),
                    removed: false,
                });
            }

            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(logs)
        })
        .expect("Failed to register method");

    let handle = server.start(module);

    (handle, addr)
}

// ========================================
// Phase 1.4: Basic Routing Integration Tests
// ========================================

#[tokio::test]
async fn test_legacy_client_creation() {
    // Start mock server
    let (_handle, addr) = create_mock_legacy_server().await;

    let config = LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    );

    let client = LegacyRpcClient::from_config(&config);
    assert!(client.is_ok());
}

#[tokio::test]
async fn test_basic_routing_get_block_by_number() {
    // Start mock server
    let (_handle, addr) = create_mock_legacy_server().await;

    let config = LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    );

    let client = LegacyRpcClient::from_config(&config).unwrap();

    // Query a block below cutoff (should work)
    let result = client
        .get_block_by_number(BlockNumberOrTag::Number(100), true)
        .await;

    assert!(result.is_ok());
    let block = result.unwrap();
    assert!(block.is_some());
    let block = block.unwrap();
    assert_eq!(block.header.number, Some(100));
}

#[tokio::test]
async fn test_legacy_client_get_transaction_by_hash() {
    let (_handle, addr) = create_mock_legacy_server().await;

    let config = LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    );

    let client = LegacyRpcClient::from_config(&config).unwrap();

    let tx_hash = B256::from([5u8; 32]);
    let result = client.get_transaction_by_hash(tx_hash).await;

    assert!(result.is_ok());
    let tx = result.unwrap();
    assert!(tx.is_some());
    // Note: hash is computed from the transaction, not set directly
}

// ========================================
// Phase 1.5: Cross-boundary getLogs Integration Tests
// ========================================

#[tokio::test]
async fn test_crossboundary_get_logs() {
    let (_handle, addr) = create_mock_legacy_server().await;

    let config = LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    );

    let client = LegacyRpcClient::from_config(&config).unwrap();

    // Query logs in legacy range
    let filter = Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(999000)),
            to_block: Some(BlockNumberOrTag::Number(999002)),
        },
        address: Default::default(),
        topics: Default::default(),
    };

    let result = client.get_logs(filter).await;
    assert!(result.is_ok());

    let logs = result.unwrap();
    assert!(!logs.is_empty());

    // Verify logs are from correct range
    for log in logs {
        assert!(log.block_number.unwrap() >= 999000);
        assert!(log.block_number.unwrap() <= 999002);
    }
}

#[tokio::test]
async fn test_filter_manager_merge_logs() {
    let (_handle, addr) = create_mock_legacy_server().await;

    let manager = CrossBoundaryFilterManager::new(1000000);
    let client = LegacyRpcClient::from_config(&LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    ))
    .unwrap();

    // Get logs from legacy range
    let legacy_filter = Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(999998)),
            to_block: Some(BlockNumberOrTag::Number(999999)),
        },
        address: Default::default(),
        topics: Default::default(),
    };

    let legacy_logs = client.get_logs(legacy_filter).await.unwrap();

    // Simulate local logs (would come from local Reth)
    let local_logs = vec![
        Log {
            inner: alloy_primitives::Log {
                address: Address::from([4u8; 20]),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: Some(B256::from([1u8; 32])),
            block_number: Some(1000000),
            block_timestamp: None,
            transaction_hash: Some(B256::from([2u8; 32])),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        },
        Log {
            inner: alloy_primitives::Log {
                address: Address::from([4u8; 20]),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: Some(B256::from([1u8; 32])),
            block_number: Some(1000001),
            block_timestamp: None,
            transaction_hash: Some(B256::from([2u8; 32])),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        },
    ];

    // Merge logs
    let merged = manager.merge_logs(legacy_logs, local_logs);

    // Verify proper ordering (legacy first, then local)
    assert!(!merged.is_empty());
    for i in 1..merged.len() {
        assert!(merged[i - 1].block_number <= merged[i].block_number);
    }
}

#[tokio::test]
async fn test_filter_classification() {
    let manager = CrossBoundaryFilterManager::new(1000000);

    // Test pure legacy filter
    let legacy_filter = Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(100)),
            to_block: Some(BlockNumberOrTag::Number(999999)),
        },
        address: Default::default(),
        topics: Default::default(),
    };

    let classification = manager.classify_filter(&legacy_filter).unwrap();
    assert_eq!(
        classification,
        reth_rpc_eth_types::FilterType::PureLegacy
    );

    // Test pure local filter
    let local_filter = Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(1000000)),
            to_block: Some(BlockNumberOrTag::Number(2000000)),
        },
        address: Default::default(),
        topics: Default::default(),
    };

    let classification = manager.classify_filter(&local_filter).unwrap();
    assert_eq!(
        classification,
        reth_rpc_eth_types::FilterType::PureLocal
    );

    // Test hybrid filter
    let hybrid_filter = Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(999000)),
            to_block: Some(BlockNumberOrTag::Number(1001000)),
        },
        address: Default::default(),
        topics: Default::default(),
    };

    let classification = manager.classify_filter(&hybrid_filter).unwrap();
    assert_eq!(
        classification,
        reth_rpc_eth_types::FilterType::Hybrid
    );
}

// ========================================
// Phase 2.3: Hash-based Fallback Tests
// ========================================

#[tokio::test]
async fn test_hash_based_query_fallback() {
    let (_handle, addr) = create_mock_legacy_server().await;

    let config = LegacyRpcConfig::new(
        1000000,
        format!("http://{}", addr),
        Duration::from_secs(30),
    );

    let client = LegacyRpcClient::from_config(&config).unwrap();

    // Test getTransactionByHash (hash-based query)
    let tx_hash = B256::from([7u8; 32]);
    let result = client.get_transaction_by_hash(tx_hash).await;

    assert!(result.is_ok());
    let tx = result.unwrap();
    assert!(tx.is_some());
}

// ========================================
// Phase 2.1: Error Handling Tests
// ========================================

#[tokio::test]
async fn test_legacy_rpc_timeout() {
    // Create a config with very short timeout
    let config = LegacyRpcConfig::new(
        1000000,
        "http://127.0.0.1:19999".to_string(), // Non-existent server
        Duration::from_millis(100), // Very short timeout
    );

    let client = LegacyRpcClient::from_config(&config).unwrap();

    // This should timeout/fail
    let result = client
        .get_block_by_number(BlockNumberOrTag::Number(100), true)
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_invalid_legacy_url() {
    let config = LegacyRpcConfig::new(
        1000000,
        "invalid-url".to_string(),
        Duration::from_secs(30),
    );

    // Creating client with invalid URL should fail
    let result = LegacyRpcClient::from_config(&config);
    assert!(result.is_err());
}

// ========================================
// Helper functions for testing
// ========================================

/// Create a test filter for a specific block range
fn create_test_filter(from: u64, to: u64) -> Filter {
    Filter {
        block_option: FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(from)),
            to_block: Some(BlockNumberOrTag::Number(to)),
        },
        address: Default::default(),
        topics: Default::default(),
    }
}

/// Verify logs are properly sorted
fn verify_log_order(logs: &[Log]) -> bool {
    for i in 1..logs.len() {
        let prev = &logs[i - 1];
        let curr = &logs[i];

        if prev.block_number > curr.block_number {
            return false;
        }

        if prev.block_number == curr.block_number {
            if prev.transaction_index > curr.transaction_index {
                return false;
            }

            if prev.transaction_index == curr.transaction_index {
                if prev.log_index > curr.log_index {
                    return false;
                }
            }
        }
    }

    true
}

#[test]
fn test_log_order_verification() {
    let logs = vec![
        Log {
            inner: alloy_primitives::Log {
                address: Address::default(),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: None,
            block_number: Some(100),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        },
        Log {
            inner: alloy_primitives::Log {
                address: Address::default(),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: None,
            block_number: Some(100),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: Some(0),
            log_index: Some(1),
            removed: false,
        },
        Log {
            inner: alloy_primitives::Log {
                address: Address::default(),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: None,
            block_number: Some(101),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        },
    ];

    assert!(verify_log_order(&logs));

    let bad_logs = vec![
        Log {
            inner: alloy_primitives::Log {
                address: Address::default(),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: None,
            block_number: Some(101),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        },
        Log {
            inner: alloy_primitives::Log {
                address: Address::default(),
                data: alloy_primitives::LogData::new_unchecked(vec![], Default::default()),
            },
            block_hash: None,
            block_number: Some(100),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        },
    ];

    assert!(!verify_log_order(&bad_logs));
}

