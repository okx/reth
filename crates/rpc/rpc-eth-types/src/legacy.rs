//! Legacy RPC support for routing historical data to legacy endpoints.
//!
//! This module provides the infrastructure to route RPC requests for blocks below
//! a cutoff point to a legacy RPC endpoint (e.g., XLayer-Erigon).

use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, TxHash, B256, U256, U64};
use alloy_rpc_types_eth::{
    AccessListResult, Block, BlockId, BlockNumberOrTag, EIP1186AccountProofResponse,
    FeeHistory, Filter, FilterChanges, FilterId, Index, Log, Transaction, TransactionReceipt,
    TransactionRequest,
};
use alloy_serde::JsonStorageKey;
use jsonrpsee::{
    core::{client::ClientT, params::ArrayParams},
    http_client::{HttpClient, HttpClientBuilder},
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

/// Configuration for legacy RPC routing.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyRpcConfig {
    /// Block number below which requests should be routed to legacy RPC.
    /// Requests for blocks >= cutoff_block are handled locally.
    pub cutoff_block: BlockNumber,

    /// Legacy RPC endpoint URL (e.g., "http://legacy-node:8545").
    pub endpoint: String,

    /// Request timeout for legacy RPC calls.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

impl LegacyRpcConfig {
    /// Create a new legacy RPC configuration.
    pub fn new(cutoff_block: BlockNumber, endpoint: String, timeout: Duration) -> Self {
        Self { cutoff_block, endpoint, timeout }
    }
}

/// HTTP client for interacting with legacy RPC endpoint.
#[derive(Debug, Clone)]
pub struct LegacyRpcClient {
    client: HttpClient,
    cutoff_block: BlockNumber,
}

impl LegacyRpcClient {
    /// Create a new legacy RPC client from configuration.
    pub fn from_config(config: &LegacyRpcConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = HttpClientBuilder::default()
            .request_timeout(config.timeout)
            .build(&config.endpoint)?;

        Ok(Self {
            client,
            cutoff_block: config.cutoff_block,
        })
    }

    /// Get the cutoff block number.
    pub fn cutoff_block(&self) -> BlockNumber {
        self.cutoff_block
    }

    /// Forward eth_getBlockByNumber to legacy RPC.
    pub async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        full: bool,
    ) -> Result<Option<Block>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBlockByNumber", (block_number, full))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getBlockByHash to legacy RPC.
    pub async fn get_block_by_hash(
        &self,
        hash: BlockHash,
        full: bool,
    ) -> Result<Option<Block>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBlockByHash", (hash, full))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getTransactionByHash to legacy RPC.
    pub async fn get_transaction_by_hash(
        &self,
        hash: TxHash,
    ) -> Result<Option<Transaction>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getTransactionByHash", (hash,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getTransactionReceipt to legacy RPC.
    pub async fn get_transaction_receipt(
        &self,
        hash: TxHash,
    ) -> Result<Option<TransactionReceipt>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getTransactionReceipt", (hash,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getLogs to legacy RPC.
    pub async fn get_logs(
        &self,
        filter: Filter,
    ) -> Result<Vec<Log>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getLogs", (filter,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_newFilter to legacy RPC.
    pub async fn new_filter(
        &self,
        filter: Filter,
    ) -> Result<FilterId, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_newFilter", (filter,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getFilterChanges to legacy RPC.
    pub async fn get_filter_changes(
        &self,
        id: FilterId,
    ) -> Result<FilterChanges, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getFilterChanges", (id,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getFilterLogs to legacy RPC.
    pub async fn get_filter_logs(
        &self,
        id: FilterId,
    ) -> Result<Vec<Log>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getFilterLogs", (id,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_uninstallFilter to legacy RPC.
    pub async fn uninstall_filter(
        &self,
        id: FilterId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_uninstallFilter", (id,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getBlockTransactionCountByNumber to legacy RPC.
    pub async fn get_block_transaction_count_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Option<U256>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBlockTransactionCountByNumber", (block_number,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getBlockTransactionCountByHash to legacy RPC.
    pub async fn get_block_transaction_count_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<U256>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBlockTransactionCountByHash", (hash,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getUncleCountByBlockNumber to legacy RPC.
    pub async fn get_uncle_count_by_block_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Option<U256>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getUncleCountByBlockNumber", (block_number,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getUncleCountByBlockHash to legacy RPC.
    pub async fn get_uncle_count_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<U256>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getUncleCountByBlockHash", (hash,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getBalance to legacy RPC.
    pub async fn get_balance(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBalance", (address, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getCode to legacy RPC.
    pub async fn get_code(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getCode", (address, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getStorageAt to legacy RPC.
    pub async fn get_storage_at(
        &self,
        address: Address,
        index: JsonStorageKey,
        block_id: Option<BlockId>,
    ) -> Result<B256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getStorageAt", (address, index, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getTransactionCount to legacy RPC.
    pub async fn get_transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getTransactionCount", (address, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_call to legacy RPC.
    pub async fn call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
    ) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_call", (request, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_estimateGas to legacy RPC.
    pub async fn estimate_gas(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
    ) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_estimateGas", (request, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_createAccessList to legacy RPC.
    pub async fn create_access_list(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
    ) -> Result<AccessListResult, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_createAccessList", (request, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getProof to legacy RPC.
    pub async fn get_proof(
        &self,
        address: Address,
        keys: Vec<B256>,
        block_id: Option<BlockId>,
    ) -> Result<EIP1186AccountProofResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getProof", (address, keys, block_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getTransactionByBlockHashAndIndex to legacy RPC.
    pub async fn get_transaction_by_block_hash_and_index(
        &self,
        hash: BlockHash,
        index: Index,
    ) -> Result<Option<Transaction>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getTransactionByBlockHashAndIndex", (hash, index))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getTransactionByBlockNumberAndIndex to legacy RPC.
    pub async fn get_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: Index,
    ) -> Result<Option<Transaction>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getTransactionByBlockNumberAndIndex", (block_number, index))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getUncleByBlockHashAndIndex to legacy RPC.
    pub async fn get_uncle_by_block_hash_and_index(
        &self,
        hash: BlockHash,
        index: Index,
    ) -> Result<Option<Block>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getUncleByBlockHashAndIndex", (hash, index))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getUncleByBlockNumberAndIndex to legacy RPC.
    pub async fn get_uncle_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: Index,
    ) -> Result<Option<Block>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getUncleByBlockNumberAndIndex", (block_number, index))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_getBlockReceipts to legacy RPC.
    pub async fn get_block_receipts(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Vec<TransactionReceipt>>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_getBlockReceipts", (block_id,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_gasPrice to legacy RPC.
    pub async fn gas_price(&self) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_gasPrice", ArrayParams::new())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_maxPriorityFeePerGas to legacy RPC.
    pub async fn max_priority_fee_per_gas(&self) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_maxPriorityFeePerGas", ArrayParams::new())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_feeHistory to legacy RPC.
    pub async fn fee_history(
        &self,
        block_count: U64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> Result<FeeHistory, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_feeHistory", (block_count, newest_block, reward_percentiles))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_blobBaseFee to legacy RPC.
    pub async fn blob_base_fee(&self) -> Result<U256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_blobBaseFee", ArrayParams::new())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Forward eth_sendRawTransaction to legacy RPC.
    pub async fn send_raw_transaction(
        &self,
        bytes: Bytes,
    ) -> Result<B256, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .request("eth_sendRawTransaction", (bytes,))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// Filter type classification for hybrid filter management.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    /// Filter only queries legacy data (to_block < cutoff).
    PureLegacy,
    /// Filter only queries local data (from_block >= cutoff).
    PureLocal,
    /// Filter spans both legacy and local data.
    Hybrid,
}

/// Metadata for a hybrid filter.
#[derive(Debug, Clone)]
pub struct FilterMetadata {
    /// Original filter specification.
    pub original_filter: Filter,
    /// Filter type.
    pub filter_type: FilterType,
    /// Legacy filter ID (if applicable).
    pub legacy_id: Option<FilterId>,
    /// Local filter ID (if applicable).
    pub local_id: Option<FilterId>,
}

/// Manager for cross-boundary filters.
#[derive(Debug, Clone)]
pub struct CrossBoundaryFilterManager {
    cutoff_block: BlockNumber,
    filters: Arc<RwLock<HashMap<FilterId, FilterMetadata>>>,
    next_id: Arc<RwLock<u64>>,
}

impl CrossBoundaryFilterManager {
    /// Create a new filter manager.
    pub fn new(cutoff_block: BlockNumber) -> Self {
        Self {
            cutoff_block,
            filters: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(RwLock::new(1)),
        }
    }

    /// Generate a new filter ID.
    fn generate_id(&self) -> FilterId {
        let mut next_id = self.next_id.write();
        let id = *next_id;
        *next_id += 1;
        FilterId::from(id)
    }

    /// Parse block range from filter.
    pub fn parse_block_range(&self, filter: &Filter) -> Result<(BlockNumber, BlockNumber), String> {
        let from = match filter.block_option.get_from_block() {
            Some(BlockNumberOrTag::Number(n)) => *n,
            Some(BlockNumberOrTag::Earliest) => 0,
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | Some(BlockNumberOrTag::Finalized) | Some(BlockNumberOrTag::Safe) | None => {
                // For pending/latest/finalized/safe/none, we assume latest block
                // In practice, this should query the current block height
                u64::MAX
            }
        };

        let to = match filter.block_option.get_to_block() {
            Some(BlockNumberOrTag::Number(n)) => *n,
            Some(BlockNumberOrTag::Earliest) => 0,
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | Some(BlockNumberOrTag::Finalized) | Some(BlockNumberOrTag::Safe) | None => {
                u64::MAX
            }
        };

        Ok((from, to))
    }

    /// Classify a filter based on its block range.
    pub fn classify_filter(&self, filter: &Filter) -> Result<FilterType, String> {
        let (from, to) = self.parse_block_range(filter)?;

        if to < self.cutoff_block {
            Ok(FilterType::PureLegacy)
        } else if from >= self.cutoff_block {
            Ok(FilterType::PureLocal)
        } else {
            Ok(FilterType::Hybrid)
        }
    }

    /// Split a hybrid filter into legacy and local parts.
    pub fn split_filter(&self, filter: &Filter) -> (Filter, Filter) {
        let mut legacy_filter = filter.clone();
        legacy_filter = legacy_filter.to_block(BlockNumberOrTag::Number(self.cutoff_block - 1));

        let mut local_filter = filter.clone();
        local_filter = local_filter.from_block(BlockNumberOrTag::Number(self.cutoff_block));

        (legacy_filter, local_filter)
    }

    /// Register a new filter.
    pub fn register_filter(
        &self,
        original_filter: Filter,
        filter_type: FilterType,
        legacy_id: Option<FilterId>,
        local_id: Option<FilterId>,
    ) -> FilterId {
        let id = self.generate_id();
        let metadata = FilterMetadata {
            original_filter,
            filter_type,
            legacy_id,
            local_id,
        };
        self.filters.write().insert(id.clone(), metadata);
        id
    }

    /// Get filter metadata.
    pub fn get_filter(&self, id: &FilterId) -> Option<FilterMetadata> {
        self.filters.read().get(id).cloned()
    }

    /// Remove a filter.
    pub fn remove_filter(&self, id: &FilterId) -> Option<FilterMetadata> {
        self.filters.write().remove(id)
    }

    /// Merge logs from legacy and local sources.
    pub fn merge_logs(&self, mut legacy_logs: Vec<Log>, mut local_logs: Vec<Log>) -> Vec<Log> {
        legacy_logs.append(&mut local_logs);
        // Sort by block number, then transaction index, then log index
        legacy_logs.sort_by(|a, b| {
            a.block_number
                .cmp(&b.block_number)
                .then(a.transaction_index.cmp(&b.transaction_index))
                .then(a.log_index.cmp(&b.log_index))
        });
        legacy_logs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types_eth::FilterBlockOption;

    // ========================================
    // Phase 1.3: Filter 管理单元测试
    // ========================================

    #[test]
    fn test_filter_manager_creation() {
        let manager = CrossBoundaryFilterManager::new(1000000);
        assert_eq!(manager.cutoff_block, 1000000);
    }

    #[test]
    fn test_classify_filter_pure_legacy() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Filter with to_block < cutoff
        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(100)),
                to_block: Some(BlockNumberOrTag::Number(999999)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let result = manager.classify_filter(&filter).unwrap();
        assert_eq!(result, FilterType::PureLegacy);
    }

    #[test]
    fn test_classify_filter_pure_local() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Filter with from_block >= cutoff
        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(1000000)),
                to_block: Some(BlockNumberOrTag::Number(2000000)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let result = manager.classify_filter(&filter).unwrap();
        assert_eq!(result, FilterType::PureLocal);
    }

    #[test]
    fn test_classify_filter_hybrid() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Filter that spans the cutoff
        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(999000)),
                to_block: Some(BlockNumberOrTag::Number(1001000)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let result = manager.classify_filter(&filter).unwrap();
        assert_eq!(result, FilterType::Hybrid);
    }

    #[test]
    fn test_classify_filter_edge_cases() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Exactly at cutoff boundary
        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(999999)),
                to_block: Some(BlockNumberOrTag::Number(1000000)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let result = manager.classify_filter(&filter).unwrap();
        assert_eq!(result, FilterType::Hybrid);
    }

    #[test]
    fn test_classify_filter_with_latest() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Filter with "latest" - should be treated as local
        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Latest),
                to_block: Some(BlockNumberOrTag::Latest),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let result = manager.classify_filter(&filter).unwrap();
        assert_eq!(result, FilterType::PureLocal);
    }

    #[test]
    fn test_split_filter() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let original_filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(999000)),
                to_block: Some(BlockNumberOrTag::Number(1001000)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let (legacy, local) = manager.split_filter(&original_filter);

        // Legacy filter should end at cutoff - 1
        if let FilterBlockOption::Range { to_block, .. } = legacy.block_option {
            assert_eq!(to_block, Some(BlockNumberOrTag::Number(999999)));
        } else {
            panic!("Expected Range filter option");
        }

        // Local filter should start at cutoff
        if let FilterBlockOption::Range { from_block, .. } = local.block_option {
            assert_eq!(from_block, Some(BlockNumberOrTag::Number(1000000)));
        } else {
            panic!("Expected Range filter option");
        }
    }

    #[test]
    fn test_register_and_get_filter() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(100)),
                to_block: Some(BlockNumberOrTag::Number(200)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let legacy_id = FilterId::from(999u64);
        let local_id = FilterId::from(1000u64);

        let id = manager.register_filter(
            filter.clone(),
            FilterType::Hybrid,
            Some(legacy_id.clone()),
            Some(local_id.clone()),
        );

        let metadata = manager.get_filter(&id).unwrap();
        assert_eq!(metadata.filter_type, FilterType::Hybrid);
        assert_eq!(metadata.legacy_id, Some(legacy_id));
        assert_eq!(metadata.local_id, Some(local_id));
    }

    #[test]
    fn test_remove_filter() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(100)),
                to_block: Some(BlockNumberOrTag::Number(200)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let id = manager.register_filter(filter, FilterType::PureLegacy, None, None);

        // Filter should exist
        assert!(manager.get_filter(&id).is_some());

        // Remove filter
        let removed = manager.remove_filter(&id);
        assert!(removed.is_some());

        // Filter should no longer exist
        assert!(manager.get_filter(&id).is_none());
    }

    #[test]
    fn test_merge_logs_sorting() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Create logs with different block numbers
        let legacy_log1 = Log {
            block_number: Some(100),
            transaction_index: Some(0),
            log_index: Some(0),
            ..Default::default()
        };

        let legacy_log2 = Log {
            block_number: Some(500),
            transaction_index: Some(1),
            log_index: Some(0),
            ..Default::default()
        };

        let local_log1 = Log {
            block_number: Some(1000000),
            transaction_index: Some(0),
            log_index: Some(0),
            ..Default::default()
        };

        let local_log2 = Log {
            block_number: Some(1000001),
            transaction_index: Some(2),
            log_index: Some(1),
            ..Default::default()
        };

        let legacy_logs = vec![legacy_log2.clone(), legacy_log1.clone()]; // Wrong order
        let local_logs = vec![local_log2.clone(), local_log1.clone()]; // Wrong order

        let merged = manager.merge_logs(legacy_logs, local_logs);

        // Should be sorted correctly
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].block_number, Some(100));
        assert_eq!(merged[1].block_number, Some(500));
        assert_eq!(merged[2].block_number, Some(1000000));
        assert_eq!(merged[3].block_number, Some(1000001));
    }

    #[test]
    fn test_merge_logs_within_same_block() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        // Logs in the same block should be sorted by transaction index and log index
        let log1 = Log {
            block_number: Some(100),
            transaction_index: Some(1),
            log_index: Some(0),
            ..Default::default()
        };

        let log2 = Log {
            block_number: Some(100),
            transaction_index: Some(0),
            log_index: Some(1),
            ..Default::default()
        };

        let log3 = Log {
            block_number: Some(100),
            transaction_index: Some(0),
            log_index: Some(0),
            ..Default::default()
        };

        let merged = manager.merge_logs(vec![log1.clone()], vec![log2.clone(), log3.clone()]);

        assert_eq!(merged.len(), 3);
        // Should be sorted by tx_index then log_index
        assert_eq!(merged[0].transaction_index, Some(0));
        assert_eq!(merged[0].log_index, Some(0));
        assert_eq!(merged[1].transaction_index, Some(0));
        assert_eq!(merged[1].log_index, Some(1));
        assert_eq!(merged[2].transaction_index, Some(1));
        assert_eq!(merged[2].log_index, Some(0));
    }

    #[test]
    fn test_parse_block_range_explicit_numbers() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(100)),
                to_block: Some(BlockNumberOrTag::Number(200)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let (from, to) = manager.parse_block_range(&filter).unwrap();
        assert_eq!(from, 100);
        assert_eq!(to, 200);
    }

    #[test]
    fn test_parse_block_range_earliest() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Earliest),
                to_block: Some(BlockNumberOrTag::Number(200)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        let (from, to) = manager.parse_block_range(&filter).unwrap();
        assert_eq!(from, 0);
        assert_eq!(to, 200);
    }

    #[test]
    fn test_filter_id_generation_unique() {
        let manager = CrossBoundaryFilterManager::new(1000000);

        let filter = Filter {
            block_option: FilterBlockOption::Range {
                from_block: Some(BlockNumberOrTag::Number(100)),
                to_block: Some(BlockNumberOrTag::Number(200)),
            },
            address: Default::default(),
            topics: Default::default(),
        };

        // Generate multiple filter IDs - should be unique
        let id1 = manager.register_filter(filter.clone(), FilterType::PureLegacy, None, None);
        let id2 = manager.register_filter(filter.clone(), FilterType::PureLegacy, None, None);
        let id3 = manager.register_filter(filter, FilterType::PureLegacy, None, None);

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }
}
