//! X Layer CLI - Command-line interface for interacting with X Layer blockchain.
//!
//! This binary provides commands for sending transactions and interacting
//! with the Reth chain via RPC.
use alloy_dyn_abi::{FunctionExt, JsonAbiExt};
use alloy_json_abi::Function;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reth_sdk::{
    create_provider, encode_args, get_balance, get_token_balance, transfer_native_asset,
    transfer_token, XAddress,
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct XlayerCli {
    #[command(subcommand)]
    command: XlayerCommands,
}

/// CLI commands for X Layer operations.
#[derive(Subcommand, Debug)]
pub enum XlayerCommands {
    /// Transfer native assets (OKB) to a specified address.
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
        /// Address to query balance for (supports optional "X" prefix, e.g., "X1234..." or
        /// "0x1234...")
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
        /// Account address to query balance for (supports optional "X" prefix, e.g., "X1234..." or
        /// "0x1234...")
        #[arg(long)]
        account: XAddress,
    },
    /// Call a contract function using eth_call RPC method.
    EthCall {
        /// RPC URL
        #[arg(long)]
        rpc_url: String,
        /// Token contract address (supports optional "X" prefix, e.g., "X1234..." or "0x1234...")
        #[arg(long)]
        to: XAddress,
        /// The signature of the function.
        #[arg(long)]
        sig: Option<String>,

        /// The arguments of the function.
        #[arg(allow_negative_numbers = true)]
        #[arg(long, num_args = 1..)]
        args: Vec<String>,
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
            transfer_token(&common.rpc_url, &common.private_key, token_address, to_address, amount)
                .await?;
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
        XlayerCommands::EthCall { rpc_url, to, sig, args } => {
            let sig = sig.unwrap();

            if sig.contains('(') {
                let func = Function::parse(&sig).unwrap();
                let values = encode_args(&func.inputs, args).unwrap();
                let data = func.abi_encode_input(&values).unwrap();

                let to_address: Address = to.into();

                let provider = create_provider(&rpc_url).await?;

                let result = provider
                    .call(alloy_rpc_types_eth::TransactionRequest {
                        to: Some(alloy_primitives::TxKind::Call(to_address)),
                        input: alloy_rpc_types_eth::TransactionInput {
                            input: None,
                            data: Some(Bytes::from(data)),
                        },
                        ..Default::default()
                    })
                    .await
                    .context("Failed to execute eth_call")?;

                match func.abi_decode_output(result.as_ref()) {
                    Ok(decoded) => {
                        println!("Result: decoded {:?}", decoded);
                    }
                    Err(err) => {
                        eprintln!("error in decode: {:?}", err);
                    }
                };
            }
        }
    }

    Ok(())
}
