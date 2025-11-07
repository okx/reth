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
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

// Define ERC20 interface using sol! macro
sol! {
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
    }
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
        /// RPC URL
        #[arg(long)]
        rpc_url: String,
        /// Private key (hex string, with or without 0x prefix)
        #[arg(long)]
        private_key: String,
        /// Recipient address
        #[arg(long)]
        to: Address,
        /// Amount to send in wei (optional, defaults to 0)
        #[arg(long)]
        amount: Option<U256>,
    },
    /// Transfer ERC20 tokens using transfer(address,uint256) function.
    TokenTransfer {
        /// RPC URL
        #[arg(long)]
        rpc_url: String,
        /// Private key (hex string, with or without 0x prefix)
        #[arg(long)]
        private_key: String,
        /// Token contract address
        #[arg(long)]
        token: Address,
        /// Recipient address
        #[arg(long)]
        to: Address,
        /// Amount to transfer (in token's smallest unit, e.g., wei for 18 decimals)
        #[arg(long)]
        amount: U256,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG=alloy_provider=debug,alloy_transport_http=debug,alloy_json_rpc=debug
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry().with(fmt::layer().with_target(false)).with(filter).init();

    let cli = XlayerCli::parse();

    match cli.command {
        XlayerCommands::Transfer { rpc_url, private_key, to, amount } => {
            transfer_native_asset(&rpc_url, &private_key, to, amount).await?;
        }
        XlayerCommands::TokenTransfer { rpc_url, private_key, token, to, amount } => {
            transfer_token(&rpc_url, &private_key, token, to, amount).await?;
        }
    }

    Ok(())
}
