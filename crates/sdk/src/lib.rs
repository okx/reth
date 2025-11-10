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
use alloy_json_abi::Param;
use alloy_dyn_abi::{DynSolType, DynSolValue};
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

/// Encode function arguments from string inputs
pub fn encode_args<I, S>(inputs: &[Param], args: I) -> Result<Vec<DynSolValue>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().collect();

    if inputs.len() != args.len() {
        anyhow::bail!("encode length mismatch: expected {} types, got {}", inputs.len(), args.len());
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
