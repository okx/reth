//! X Layer CLI - Command-line interface for interacting with X Layer blockchain.
//!
//! This binary provides commands for sending transactions and interacting
//! with the Reth chain via RPC.
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::str::FromStr;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

/// Common fields shared across transfer commands
#[derive(Parser, Debug)]
pub struct CommonTransferArgs {
    /// RPC URL
    #[arg(long)]
    rpc_url: String,
    /// Private key (hex string, with or without 0x prefix)
    #[arg(long)]
    private_key: String,
}

/// Create a provider with wallet from RPC URL and private key
async fn create_provider_with_wallet(
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
async fn create_provider(rpc_url: &str) -> Result<impl Provider<alloy_network::Ethereum> + Clone> {
    let provider = ProviderBuilder::new()
        .connect_http(rpc_url.parse().context("Invalid RPC URL")?);
    Ok(provider)
}

/// Transfer native assets (ETH) to an address
async fn transfer_native_asset(
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
async fn transfer_token(
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
async fn get_balance(
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
async fn get_token_balance(
    rpc_url: &str,
    token_address: Address,
    account_address: Address,
) -> Result<()> {
    let provider = create_provider(rpc_url).await?;

    let call = IERC20::balanceOfCall { account: account_address };
    let calldata = call.abi_encode();

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

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct XlayerCli {
    #[command(subcommand)]
    command: XlayerCommands,
}

/// CLI commands for X Layer operations.
#[derive(Subcommand, Debug)]
pub enum XlayerCommands {
    /// Transfer native assets (ETH) to a specified address.
    Transfer {
        /// Common transfer arguments (RPC URL and private key)
        #[command(flatten)]
        common: CommonTransferArgs,
        /// Recipient address (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        to: XAddress,
        /// Amount to send in wei (optional, defaults to 0)
        #[arg(long)]
        amount: Option<U256>,
    },
    /// Transfer ERC20 tokens using transfer(address,uint256) function.
    TokenTransfer {
        /// Common transfer arguments (RPC URL and private key)
        #[command(flatten)]
        common: CommonTransferArgs,
        /// Token contract address (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        token: XAddress,
        /// Recipient address (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        to: XAddress,
        /// Amount to transfer (in token's smallest unit, e.g., wei for 18 decimals)
        #[arg(long)]
        amount: U256,
    },
    /// Get the balance of an address (equivalent to eth_getBalance).
    Balance {
        /// RPC URL
        #[arg(long)]
        rpc_url: String,
        /// Address to query balance for (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        address: XAddress,
    },
    /// Get the ERC20 token balance of an address (equivalent to calling balanceOf(address)).
    TokenBalance {
        /// RPC URL
        #[arg(long)]
        rpc_url: String,
        /// Token contract address (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        token: XAddress,
        /// Account address to query balance for (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        account: XAddress,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG=alloy_provider=debug,alloy_transport_http=debug,alloy_json_rpc=debug
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry().with(fmt::layer().with_target(false)).with(filter).init();

    let cli = XlayerCli::parse();

    match cli.command {
        XlayerCommands::Transfer { common, to, amount } => {
            let to_address: Address = to.into();
            transfer_native_asset(&common.rpc_url, &common.private_key, to_address, amount).await?;
        }
        XlayerCommands::TokenTransfer { common, token, to, amount } => {
            let token_address: Address = token.into();
            let to_address: Address = to.into();
            transfer_token(&common.rpc_url, &common.private_key, token_address, to_address, amount).await?;
        }
        XlayerCommands::Balance { rpc_url, address } => {
            let address: Address = address.into();
            get_balance(&rpc_url, address).await?;
        }
        XlayerCommands::TokenBalance { rpc_url, token, account } => {
            let token_address: Address = token.into();
            let account_address: Address = account.into();
            get_token_balance(&rpc_url, token_address, account_address).await?;
        }
    }

    Ok(())
}
