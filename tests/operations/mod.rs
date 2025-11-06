//! RPC operations for testing

mod rpc_all;
mod test_utils_xlayer;
pub mod manager;
/// Contract bytecode and ABI constants
pub mod constants_xlayer;

pub use rpc_all::{
    debug_trace_block_by_hash, debug_trace_block_by_number, debug_trace_transaction, estimate_gas,
    eth_block_number, eth_call, eth_chain_id, eth_gas_price, eth_get_block_by_hash,
    eth_get_block_by_number, eth_get_block_receipts, eth_get_block_transaction_count_by_hash,
    eth_get_block_transaction_count_by_number, eth_get_code, eth_get_logs, eth_get_storage_at,
    eth_get_transaction_by_block_hash_and_index, eth_get_transaction_by_block_number_and_index,
    eth_get_transaction_by_hash, eth_get_transaction_count, eth_get_transaction_receipt,
    eth_syncing, get_balance, txpool_content, txpool_status, BlockId,
};
pub use test_utils_xlayer::{
    deploy_contract, ensure_contracts_deployed, erc20_transfer_tx, get_refund_counter_from_trace,
    setup_test_environment, sign_and_send_transaction, transfer_erc20_token_batch, transfer_token,
    transfer_token_with_from, wait_for_blocks, DeployedContracts,
};

use jsonrpsee::http_client::HttpClient;

/// Creates an HTTP client for the local test RPC endpoint
/// 
/// # Panics
/// Panics if the client cannot be created (e.g., invalid URL)
pub fn create_test_client() -> HttpClient {
    HttpClient::builder()
        .build("http://localhost:8124")
        .expect("Failed to create HTTP client")
}

/// Creates an HTTP client for a custom RPC endpoint
/// 
/// # Panics
/// Panics if the client cannot be created (e.g., invalid URL)
pub fn create_client(url: &str) -> HttpClient {
    HttpClient::builder()
        .build(url)
        .expect("Failed to create HTTP client")
}
