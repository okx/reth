//! X Layer CLI - Command-line interface for interacting with X Layer blockchain.
//!
//! This binary provides commands for sending transactions and interacting
//! with the Reth chain via RPC.
use alloy_dyn_abi::{DynSolType, DynSolValue, FunctionExt, JsonAbiExt};
use alloy_json_abi::{Function, Param};
use alloy_primitives::{hex, Address, Bytes, U256};
use alloy_provider::Provider;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reth_sdk::{
    create_provider, get_balance, get_token_balance, transfer_native_asset, transfer_token,
    XAddress,
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
                println!("result: {:?}", result);
                let decoded = match func.abi_decode_output(result.as_ref()) {
                    Ok(decoded) => decoded, // Vec<DynSolValue>
                    Err(err) => {
                        panic!("error in decoding")
                    }
                };
                println!("Result: decoded {:?}", decoded);
            }
        }
    }

    Ok(())
}

pub fn encode_args<I, S>(inputs: &[Param], args: I) -> Result<Vec<DynSolValue>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().collect();

    if inputs.len() != args.len() {
        panic!("encode length mismatch: expected {} types, got {}", inputs.len(), args.len())
    }

    std::iter::zip(inputs, args)
        .map(|(input, arg)| coerce_value(&input.selector_type(), arg.as_ref()))
        .collect()
}

/// Helper function to coerce a value to a [DynSolValue] given a type string
pub fn coerce_value(ty: &str, arg: &str) -> Result<DynSolValue> {
    println!("type {:?}, arg: {:?}", ty, arg);

    // Parse the type first to see if it's a tuple
    let parsed_ty = DynSolType::parse(ty)?;

    // Recursively process based on the type structure
    coerce_value_recursive(&parsed_ty, arg)
}

/// Recursive helper to process types and strip X prefix from addresses
fn coerce_value_recursive(ty: &DynSolType, arg: &str) -> Result<DynSolValue> {
    match ty {
        DynSolType::Tuple(tuple_types) => {
            // Parse tuple arguments (handle nested tuples)
            let args = parse_tuple_args(arg)?;

            if tuple_types.len() != args.len() {
                anyhow::bail!("Tuple length mismatch: expected {} elements, got {}", tuple_types.len(), args.len());
            }

            // Recursively process each element
            let values: Result<Vec<_>> = tuple_types
                .iter()
                .zip(args.iter())
                .map(|(ty, arg_str)| coerce_value_recursive(ty, arg_str))
                .collect();

            Ok(DynSolValue::Tuple(values?))
        }
        DynSolType::Array(inner_ty) => {
            // Parse array arguments (comma-separated, optionally wrapped in brackets)
            let args = parse_array_args(arg)?;

            // Recursively process each element
            let values: Result<Vec<_>> = args
                .iter()
                .map(|arg_str| coerce_value_recursive(inner_ty, arg_str))
                .collect();

            Ok(DynSolValue::Array(values?))
        }
        DynSolType::FixedArray(inner_ty, size) => {
            // Parse fixed array arguments (comma-separated, optionally wrapped in brackets)
            let args = parse_array_args(arg)?;

            if args.len() != *size {
                anyhow::bail!("Fixed array length mismatch: expected {} elements, got {}", size, args.len());
            }

            // Recursively process each element
            let values: Result<Vec<_>> = args
                .iter()
                .map(|arg_str| coerce_value_recursive(inner_ty, arg_str))
                .collect();

            Ok(DynSolValue::FixedArray(values?))
        }
        DynSolType::Address => {
            // Strip X prefix from address
            let addr_str = arg.strip_prefix('X').unwrap_or(arg);
            let addr_with_prefix = if addr_str.starts_with("0x") {
                addr_str.to_string()
            } else {
                format!("0x{}", addr_str)
            };
            DynSolType::coerce_str(&DynSolType::Address, &addr_with_prefix)
                .context("Failed to coerce address")
        }
        _ => {
            // For non-tuple, non-address types, use normal coercion
            DynSolType::coerce_str(ty, arg)
                .context("Failed to coerce value")
        }
    }
}

/// Parse tuple arguments from a string, handling nested tuples
fn parse_tuple_args(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        anyhow::bail!("Tuple argument must start with '(' and end with ')'");
    }

    let inner = &s[1..s.len()-1]; // Remove outer parentheses
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0;

    for ch in inner.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if !current.is_empty() {
        args.push(current.trim().to_string());
    }

    Ok(args)
}

/// Parse array arguments from a string (comma-separated, optionally wrapped in brackets)
fn parse_array_args(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    
    // Remove optional brackets
    let inner = if s.starts_with('[') && s.ends_with(']') {
        &s[1..s.len()-1]
    } else {
        s
    };
    
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    
    // Split by comma, handling nested structures
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    
    for ch in inner.chars() {
        match ch {
            '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }
    
    if !current.is_empty() {
        args.push(current.trim().to_string());
    }
    
    Ok(args)
}
