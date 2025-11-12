//! Reth SDK for interacting with X Layer blockchain.
//!
//! This crate provides utilities and functions for sending transactions
//! and interacting with the Reth chain via RPC.

use alloy_dyn_abi::{DynSolType, DynSolValue};
use alloy_json_abi::Param;
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall};
use anyhow::{Context, Result};
use std::str::FromStr;

/// Prefix character for X Layer addresses
const X_ADDRESS_PREFIX: char = 'X';

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
        let addr_str = s.strip_prefix(X_ADDRESS_PREFIX).unwrap_or(s);
        let addr_with_prefix =
            if addr_str.starts_with("0x") { addr_str.to_string() } else { format!("0x{addr_str}") };

        let address = addr_with_prefix.parse::<Address>().context("Failed to parse address")?;
        Ok(Self { address })
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
pub async fn create_provider(
    rpc_url: &str,
) -> Result<impl Provider<alloy_network::Ethereum> + Clone> {
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse().context("Invalid RPC URL")?);
    Ok(provider)
}

/// Transfer native assets (OKB) to an address
pub async fn transfer_native_asset(
    rpc_url: &str,
    private_key: &str,
    to_address: Address,
    amount: Option<U256>,
) -> Result<B256> {
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

    Ok(receipt.transaction_hash)
}

/// Transfer ERC20 tokens using transfer(address,uint256) function
pub async fn transfer_token(
    rpc_url: &str,
    private_key: &str,
    token_address: Address,
    recipient: Address,
    amount: U256,
) -> Result<B256> {
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

    Ok(receipt.transaction_hash)
}

/// Get the balance of an address
pub async fn get_balance(rpc_url: &str, address: Address) -> Result<U256> {
    let provider = create_provider(rpc_url).await?;
    let balance = provider.get_balance(address).await.context("Failed to get balance")?;
    Ok(balance)
}

/// Get the ERC20 token balance of an address
pub async fn get_token_balance(
    rpc_url: &str,
    token_address: Address,
    account_address: Address,
) -> Result<U256> {
    let provider = create_provider(rpc_url).await?;

    let call = IERC20::balanceOfCall { account: account_address };
    let calldata = call.abi_encode();
    let result = provider
        .call(alloy_rpc_types_eth::TransactionRequest {
            to: Some(alloy_primitives::TxKind::Call(token_address)),
            input: alloy_rpc_types_eth::TransactionInput {
                input: None,
                data: Some(Bytes::from(calldata)),
            },
            ..Default::default()
        })
        .await
        .context("Failed to call balanceOf")?;

    let balance = IERC20::balanceOfCall::abi_decode_returns(&result.0)
        .context("Failed to decode balanceOf return value")?;

    Ok(balance)
}

/// Encode function arguments from string inputs
pub fn encode_args<I, S>(inputs: &[Param], args: I) -> Result<Vec<DynSolValue>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().collect();

    if inputs.len() != args.len() {
        anyhow::bail!(
            "encode length mismatch: expected {} types, got {}",
            inputs.len(),
            args.len()
        );
    }

    std::iter::zip(inputs, args)
        .map(|(input, arg)| coerce_value(&input.selector_type(), arg.as_ref()))
        .collect()
}

/// Helper function to coerce a value to a [`DynSolValue`] given a type string
pub fn coerce_value(ty: &str, arg: &str) -> Result<DynSolValue> {
    let parsed_ty = DynSolType::parse(ty)?;
    coerce_value_recursive(&parsed_ty, arg)
}

/// Recursive helper to process types and strip X prefix from addresses
fn coerce_value_recursive(ty: &DynSolType, arg: &str) -> Result<DynSolValue> {
    match ty {
        DynSolType::Tuple(tuple_types) => {
            let args = parse_tuple_args(arg)?;

            if tuple_types.len() != args.len() {
                anyhow::bail!(
                    "Tuple length mismatch: expected {} elements, got {}",
                    tuple_types.len(),
                    args.len()
                );
            }
            let values: Result<Vec<_>> = tuple_types
                .iter()
                .zip(args.iter())
                .map(|(ty, arg_str)| coerce_value_recursive(ty, arg_str))
                .collect();

            Ok(DynSolValue::Tuple(values?))
        }
        DynSolType::Array(inner_ty) => {
            let args = parse_array_args(arg)?;
            let values: Result<Vec<_>> =
                args.iter().map(|arg_str| coerce_value_recursive(inner_ty, arg_str)).collect();

            Ok(DynSolValue::Array(values?))
        }
        DynSolType::FixedArray(inner_ty, size) => {
            let args = parse_array_args(arg)?;
            if args.len() != *size {
                anyhow::bail!(
                    "Fixed array length mismatch: expected {} elements, got {}",
                    size,
                    args.len()
                );
            }
            let values: Result<Vec<_>> =
                args.iter().map(|arg_str| coerce_value_recursive(inner_ty, arg_str)).collect();

            Ok(DynSolValue::FixedArray(values?))
        }
        DynSolType::Address => {
            // Strip X prefix from address
            let addr_str = arg.strip_prefix(X_ADDRESS_PREFIX).unwrap_or(arg);
            let addr_with_prefix = if addr_str.starts_with("0x") {
                addr_str.to_string()
            } else {
                format!("0x{addr_str}")
            };
            DynSolType::coerce_str(&DynSolType::Address, &addr_with_prefix)
                .context("Failed to coerce address")
        }
        _ => {
            // For non-tuple, non-address types, use normal coercion
            DynSolType::coerce_str(ty, arg).context("Failed to coerce value")
        }
    }
}

/// Parse tuple arguments from a string, handling nested tuples
fn parse_tuple_args(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        anyhow::bail!("Tuple argument must start with '(' and end with ')'");
    }

    let inner = &s[1..s.len() - 1]; // Remove outer parentheses
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
    let inner = if s.starts_with('[') && s.ends_with(']') { &s[1..s.len() - 1] } else { s };

    if inner.is_empty() {
        return Ok(Vec::new());
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_abi::Function;

    #[test]
    fn test_encode_args_with_xaddress_in_tuple() {
        // Test the example from the CLI command:
        // struct QuoteExactInputSingleParams {
        //     address tokenIn;
        //     address tokenOut;
        //     uint24 fee;
        //     uint256 amountIn;
        //     uint160 sqrtPriceLimitX96;
        // }
        // quoteExactInputSingle((address,address,uint256,uint24,uint160))(uint256,uint160,uint32,
        // uint256) with args:
        // (XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7,
        // 1000000000000000000,3000,0)

        let sig = "quoteExactInputSingle((address,address,uint256,uint24,uint160))(uint256,uint160,uint32,uint256)";
        let func = Function::parse(sig).unwrap();

        let args = vec!["(XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7,1000000000000000000,3000,0)"];

        let result = encode_args(&func.inputs, args).unwrap();

        assert_eq!(result.len(), 1);

        if let DynSolValue::Tuple(tuple_values) = &result[0] {
            assert_eq!(tuple_values.len(), 5);

            // Check first address (should have X prefix stripped)
            if let DynSolValue::Address(addr1) = &tuple_values[0] {
                assert_eq!(addr1.to_string(), "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
            } else {
                panic!("First element should be an address");
            }

            // Check second address (should have X prefix stripped)
            if let DynSolValue::Address(addr2) = &tuple_values[1] {
                assert_eq!(addr2.to_string(), "0xdAC17F958D2ee523a2206206994597C13D831ec7");
            } else {
                panic!("Second element should be an address");
            }

            // Check uint256
            if let DynSolValue::Uint(amount, 256) = &tuple_values[2] {
                assert_eq!(amount.to_string(), "1000000000000000000");
            } else {
                panic!("Third element should be uint256");
            }

            // Check uint24
            if let DynSolValue::Uint(fee, 24) = &tuple_values[3] {
                assert_eq!(fee.to_string(), "3000");
            } else {
                panic!("Fourth element should be uint24");
            }

            // Check uint160
            if let DynSolValue::Uint(sqrt_price, 160) = &tuple_values[4] {
                assert_eq!(sqrt_price.to_string(), "0");
            } else {
                panic!("Fifth element should be uint160");
            }
        } else {
            panic!("Result should be a tuple");
        }
    }

    #[test]
    fn test_encode_args_with_nested_tuple() {
        // Test nested tuple: (address,(address,uint256))
        // Function signature: testFunction((address,(address,uint256)))(uint256)
        // Args: (XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,
        // (XdAC17F958D2ee523a2206206994597C13D831ec7,1000))

        let sig = "testFunction((address,(address,uint256)))(uint256)";
        let func = Function::parse(sig).unwrap();

        let args = vec!["(XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,(XdAC17F958D2ee523a2206206994597C13D831ec7,1000))"];

        let result = encode_args(&func.inputs, args).unwrap();

        assert_eq!(result.len(), 1);

        if let DynSolValue::Tuple(outer_tuple) = &result[0] {
            assert_eq!(outer_tuple.len(), 2);

            // Check first element (address)
            if let DynSolValue::Address(addr1) = &outer_tuple[0] {
                assert_eq!(addr1.to_string(), "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
            } else {
                panic!("First element should be an address");
            }

            // Check second element (nested tuple)
            if let DynSolValue::Tuple(inner_tuple) = &outer_tuple[1] {
                assert_eq!(inner_tuple.len(), 2);

                // Check inner tuple's first element (address)
                if let DynSolValue::Address(addr2) = &inner_tuple[0] {
                    assert_eq!(addr2.to_string(), "0xdAC17F958D2ee523a2206206994597C13D831ec7");
                } else {
                    panic!("Inner tuple's first element should be an address");
                }

                // Check inner tuple's second element (uint256)
                if let DynSolValue::Uint(amount, 256) = &inner_tuple[1] {
                    assert_eq!(amount.to_string(), "1000");
                } else {
                    panic!("Inner tuple's second element should be uint256");
                }
            } else {
                panic!("Second element should be a tuple");
            }
        } else {
            panic!("Result should be a tuple");
        }
    }
    #[test]
    fn test_encode_args_with_dynamic_array() {
        // Test dynamic array without brackets: address[]
        // Function signature: testFunction(address[])(uint256)
        // Args: XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7
        // (comma-separated without brackets)

        let sig = "testFunction(address[])(uint256)";
        let func = Function::parse(sig).unwrap();

        let args = vec![
            "XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7",
        ];

        let result = encode_args(&func.inputs, args).unwrap();

        assert_eq!(result.len(), 1);

        if let DynSolValue::Array(array_values) = &result[0] {
            assert_eq!(array_values.len(), 2);

            // Check first address (should have X prefix stripped)
            if let DynSolValue::Address(addr1) = &array_values[0] {
                assert_eq!(addr1.to_string(), "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
            } else {
                panic!("First element should be an address");
            }

            // Check second address (should have X prefix stripped)
            if let DynSolValue::Address(addr2) = &array_values[1] {
                assert_eq!(addr2.to_string(), "0xdAC17F958D2ee523a2206206994597C13D831ec7");
            } else {
                panic!("Second element should be an address");
            }
        } else {
            panic!("Result should be an array");
        }
    }

    #[test]
    fn test_encode_args_with_fixed_array() {
        // Test fixed-size array: address[3]
        // Function signature: testFunction(address[3])(uint256)
        // Args: [XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,
        // XdAC17F958D2ee523a2206206994597C13D831ec7,0x1234567890123456789012345678901234567890]

        let sig = "testFunction(address[3])(uint256)";
        let func = Function::parse(sig).unwrap();

        let args = vec!["[XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7,0x1234567890123456789012345678901234567890]"];

        let result = encode_args(&func.inputs, args).unwrap();

        assert_eq!(result.len(), 1);

        if let DynSolValue::FixedArray(array_values) = &result[0] {
            assert_eq!(array_values.len(), 3);

            // Check first address (should have X prefix stripped)
            if let DynSolValue::Address(addr1) = &array_values[0] {
                assert_eq!(addr1.to_string(), "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
            } else {
                panic!("First element should be an address");
            }

            // Check second address (should have X prefix stripped)
            if let DynSolValue::Address(addr2) = &array_values[1] {
                assert_eq!(addr2.to_string(), "0xdAC17F958D2ee523a2206206994597C13D831ec7");
            } else {
                panic!("Second element should be an address");
            }

            // Check third address (without X prefix, should still work)
            if let DynSolValue::Address(addr3) = &array_values[2] {
                assert_eq!(addr3.to_string(), "0x1234567890123456789012345678901234567890");
            } else {
                panic!("Third element should be an address");
            }
        } else {
            panic!("Result should be a fixed array");
        }
    }

    #[test]
    fn test_encode_args_with_fixed_array_wrong_length() {
        // Test fixed-size array with wrong length: address[3] but only 2 elements provided
        // This should fail with a length mismatch error

        let sig = "testFunction(address[3])(uint256)";
        let func = Function::parse(sig).unwrap();

        let args = vec![
            "[XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7]",
        ];

        let result = encode_args(&func.inputs, args);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Fixed array length mismatch"));
    }
}
