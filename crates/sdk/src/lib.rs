//! Reth SDK for interacting with X Layer blockchain.
//!
//! This crate provides utilities and functions for sending transactions
//! and interacting with the Reth chain via RPC.

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes, U256, hex};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result};
use std::str::FromStr;

// Define ERC20 interface using sol! macro
sol! {
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Address type that handles addresses with an optional "X" prefix
/// The "X" prefix is stripped when converting to Address for chain operations
/// Examples: "X1234..." or "0x1234..." or "1234..." (all valid)
#[derive(Debug, Clone)]
pub struct XAddress {
    address: Address,
}

impl FromStr for XAddress {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let addr_str = s.strip_prefix('X').unwrap_or(s);
        let addr_with_prefix = if addr_str.starts_with("0x") {
            addr_str.to_string()
        } else {
            format!("0x{}", addr_str)
        };

        let address = addr_with_prefix.parse::<Address>()
            .context("Failed to parse address")?;
        Ok(XAddress { address })
    }
}

impl From<XAddress> for Address {
    fn from(x_addr: XAddress) -> Self {
        x_addr.address
    }
}

impl std::fmt::Display for XAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.address)
    }
}

/// Create a provider with wallet from RPC URL and private key
pub async fn create_provider_with_wallet(
    rpc_url: &str,
    private_key: &str,
) -> Result<impl Provider<alloy_network::Ethereum> + Clone> {
    let private_key = private_key.strip_prefix("0x").unwrap_or(private_key);
    let signer: PrivateKeySigner = private_key.parse().context("Failed to parse private key")?;
    let wallet = EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse().context("Invalid RPC URL")?);

    Ok(provider)
}

/// Create a provider without wallet (for read-only operations)
pub async fn create_provider(rpc_url: &str) -> Result<impl Provider<alloy_network::Ethereum> + Clone> {
    let provider = ProviderBuilder::new()
        .connect_http(rpc_url.parse().context("Invalid RPC URL")?);
    Ok(provider)
}

/// Transfer native assets (OKB) to an address
pub async fn transfer_native_asset(
    rpc_url: &str,
    private_key: &str,
    to_address: Address,
    amount: Option<U256>,
) -> Result<()> {
    let provider = create_provider_with_wallet(rpc_url, private_key).await?;

    let chain_id = provider.get_chain_id().await.context("Failed to get chain ID")?;
    let gas_price = provider.get_gas_price().await.context("Failed to get gas price")?;

    let tx_request = TransactionRequest {
        to: Some(alloy_primitives::TxKind::Call(to_address)),
        value: amount,
        gas: Some(21000u64),
        gas_price: Some(gas_price),
        chain_id: Some(chain_id),
        ..Default::default()
    };

    let pending_tx =
        provider.send_transaction(tx_request).await.context("Failed to send transaction")?;

    let receipt = pending_tx.get_receipt().await?;

    println!("✅ Transaction sent! Hash: {:?}", receipt.transaction_hash);

    Ok(())
}

/// Transfer ERC20 tokens using transfer(address,uint256) function
pub async fn transfer_token(
    rpc_url: &str,
    private_key: &str,
    token_address: Address,
    recipient: Address,
    amount: U256,
) -> Result<()> {
    let provider = create_provider_with_wallet(rpc_url, private_key).await?;

    let chain_id = provider.get_chain_id().await.context("Failed to get chain ID")?;
    let gas_price = provider.get_gas_price().await.context("Failed to get gas price")?;

    let call = IERC20::transferCall { to: recipient, amount };

    // This produces the calldata automatically:
    let calldata = call.abi_encode();

    let tx_request = TransactionRequest {
        to: Some(alloy_primitives::TxKind::Call(token_address)),
        input: alloy_rpc_types_eth::TransactionInput {
            input: None,
            data: Some(Bytes::from(calldata)),
        },
        gas: Some(100000u64), // ERC20 transfers typically need more gas than native transfers
        gas_price: Some(gas_price),
        chain_id: Some(chain_id),
        ..Default::default()
    };

    let pending_tx =
        provider.send_transaction(tx_request).await.context("Failed to send transaction")?;

    let receipt = pending_tx.get_receipt().await?;

    println!("✅ Token transfer transaction sent! Hash: {:?}", receipt.transaction_hash);

    Ok(())
}

/// Get the balance of an address
pub async fn get_balance(
    rpc_url: &str,
    address: Address,
) -> Result<()> {
    let provider = create_provider(rpc_url).await?;

    let balance = provider.get_balance(address)
        .await
        .context("Failed to get balance")?;

    println!("Balance: {} wei", balance);

    Ok(())
}

/// Get the ERC20 token balance of an address
pub async fn get_token_balance(
    rpc_url: &str,
    token_address: Address,
    account_address: Address,
) -> Result<()> {
    let provider = create_provider(rpc_url).await?;

    let call = IERC20::balanceOfCall { account: account_address };
    let calldata = call.abi_encode();
   println!("call data: 0x{}", hex::encode(&calldata));
    let result = provider
        .call(
            alloy_rpc_types_eth::TransactionRequest {
                to: Some(alloy_primitives::TxKind::Call(token_address)),
                input: alloy_rpc_types_eth::TransactionInput {
                    input: None,
                    data: Some(Bytes::from(calldata)),
                },
                ..Default::default()
            }
        )
        .await
        .context("Failed to call balanceOf")?;

    let balance = IERC20::balanceOfCall::abi_decode_returns(&result.0)
        .context("Failed to decode balanceOf return value")?;

    println!("Token Balance: {} (raw units)", balance);

    Ok(())
}
