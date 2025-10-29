//! Legacy RPC routing support.

use std::sync::Arc;

/// Trait for providing access to legacy RPC client for routing historical data.
pub trait LegacyRpc {
    /// Returns the legacy RPC client if configured.
    fn legacy_rpc_client(&self) -> Option<&Arc<reth_rpc_eth_types::LegacyRpcClient>>;

    /// Returns the legacy filter manager if configured.
    fn legacy_filter_manager(
        &self,
    ) -> Option<&Arc<reth_rpc_eth_types::CrossBoundaryFilterManager>>;

    /// Check if a block number should be routed to legacy RPC.
    fn should_route_to_legacy(&self, block_number: u64) -> bool {
        if let Some(client) = self.legacy_rpc_client() {
            block_number < client.cutoff_block()
        } else {
            false
        }
    }
}

