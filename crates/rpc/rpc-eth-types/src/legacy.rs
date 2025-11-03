//! Legacy RPC support for routing historical data to legacy endpoints.
//!
//! This module provides the infrastructure to route RPC requests for blocks below
//! a cutoff point to a legacy RPC endpoint (e.g., XLayer-Erigon).

use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, TxHash, B256, U256, U64};
use alloy_rpc_types_eth::{
    AccessListResult, Block, BlockId, BlockNumberOrTag, EIP1186AccountProofResponse,
    FeeHistory, Filter, Index, Log, Transaction, TransactionReceipt,
    TransactionRequest,
};
use alloy_serde::JsonStorageKey;
use jsonrpsee::{
    core::{client::ClientT, params::ArrayParams},
    http_client::{HttpClient, HttpClientBuilder},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_rpc_config() {
        let config = LegacyRpcConfig::new(
            1000000,
            "http://legacy:8545".to_string(),
            std::time::Duration::from_secs(30),
        );
        assert_eq!(config.cutoff_block, 1000000);
        assert_eq!(config.endpoint, "http://legacy:8545");
    }
}
