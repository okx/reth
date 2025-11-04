//! XLayer-specific extensions for EthFilter

use super::super::EthFilter;
use alloy_rpc_types_eth::{Filter, Log};
use jsonrpsee::core::RpcResult;
use reth_rpc_eth_api::{helpers::{internal_rpc_err, LegacyRpc}, EthApiTypes, FullEthApiTypes, RpcNodeCoreExt};
use reth_rpc_eth_types::LegacyRpcClient;
use reth_storage_api::{BlockIdReader, BlockReader};
use std::sync::Arc;
use tracing::info;

/// XLayer: Implement LegacyRpc trait for EthFilter to enable legacy RPC routing
impl<Eth> LegacyRpc for EthFilter<Eth>
where
    Eth: LegacyRpc + EthApiTypes,
{
    fn legacy_rpc_client(&self) -> Option<&Arc<LegacyRpcClient>> {
        self.inner.eth_api.legacy_rpc_client()
    }
}

/// XLayer: Legacy RPC routing methods
impl<Eth> EthFilter<Eth>
where
    Eth: FullEthApiTypes<Provider: BlockReader + BlockIdReader> + RpcNodeCoreExt + LegacyRpc + 'static,
{
    /// Parse block range from filter for legacy routing logic
    pub(super) fn parse_block_range(&self, filter: &Filter) -> RpcResult<(u64, u64)> {
        let from = match filter.block_option.get_from_block() {
            Some(alloy_rpc_types_eth::BlockNumberOrTag::Number(n)) => *n,
            Some(alloy_rpc_types_eth::BlockNumberOrTag::Earliest) => 0,
            _ => {
                // For latest/pending/finalized/safe, use current block
                // This is a rough approximation - in production you'd query the actual block number
                u64::MAX
            }
        };

        let to = match filter.block_option.get_to_block() {
            Some(alloy_rpc_types_eth::BlockNumberOrTag::Number(n)) => *n,
            Some(alloy_rpc_types_eth::BlockNumberOrTag::Earliest) => 0,
            _ => u64::MAX,
        };

        Ok((from, to))
    }

    /// Check if eth_getLogs needs legacy RPC routing and handle accordingly
    ///
    /// Returns:
    /// - `Some(result)` if legacy/hybrid routing is used (query complete)
    /// - `None` if pure local processing should be used (no legacy needed)
    pub(super) async fn route_logs_to_legacy(&self, filter: Filter) -> Option<RpcResult<Vec<Log>>> {
        // Check if legacy RPC routing is configured
        let legacy_client = self.inner.eth_api.legacy_rpc_client()?;
        let cutoff_block = legacy_client.cutoff_block();

        // Parse block range from filter
        let (from_block, to_block) = match self.parse_block_range(&filter) {
            Ok(range) => range,
            Err(e) => return Some(Err(e)),
        };

        // Determine routing strategy
        if to_block < cutoff_block {
            // Pure legacy: all blocks are below cutoff
            info!(target: "rpc::eth::legacy", method = "eth_getLogs", from = from_block, to = to_block, "→ legacy");
            match reth_rpc_eth_api::helpers::exec_legacy("eth_getLogs", legacy_client.get_logs(filter)).await {
                Ok(logs) => return Some(Ok(logs)),
                Err(e) => return Some(Err(internal_rpc_err(e))),
            }
        } else if from_block >= cutoff_block {
            // Pure local: all blocks are at or above cutoff
            // Return None to signal local processing
            return None;
        } else {
            // Hybrid: spans both legacy and local ranges
            let start = std::time::Instant::now();
            info!(target: "rpc::eth::legacy", method = "eth_getLogs", from = from_block, to = to_block, "→ hybrid");

            // Split filter into legacy and local parts
            let mut legacy_filter = filter.clone();
            legacy_filter = legacy_filter.to_block(alloy_rpc_types_eth::BlockNumberOrTag::Number(cutoff_block - 1));
            let mut local_filter = filter;
            local_filter = local_filter.from_block(alloy_rpc_types_eth::BlockNumberOrTag::Number(cutoff_block));

            // Query both in parallel
            let (legacy_result, local_result) = tokio::join!(
                async {
                    legacy_client.get_logs(legacy_filter).await
                },
                async {
                    self.logs_for_filter(local_filter, self.inner.query_limits).await
                }
            );

            let mut legacy_logs = match legacy_result.map_err(|e| internal_rpc_err(e)) {
                Ok(logs) => logs,
                Err(e) => return Some(Err(e)),
            };

            let legacy_count = legacy_logs.len();
            let mut local_logs = match local_result {
                Ok(logs) => logs,
                Err(e) => return Some(Err(e.into())),
            };

            let local_count = local_logs.len();

            // Merge and sort logs
            legacy_logs.append(&mut local_logs);
            legacy_logs.sort_by(|a, b| {
                a.block_number
                    .cmp(&b.block_number)
                    .then(a.transaction_index.cmp(&b.transaction_index))
                    .then(a.log_index.cmp(&b.log_index))
            });

            info!(target: "rpc::eth::legacy", method = "eth_getLogs", elapsed_ms = %start.elapsed().as_millis(),
                  legacy_logs = legacy_count, local_logs = local_count, total = legacy_logs.len(), "← hybrid");

            return Some(Ok(legacy_logs));
        }
    }
}

