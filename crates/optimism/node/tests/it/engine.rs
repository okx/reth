use std::hash::Hash;
use alloy_primitives::{keccak256,hex, Bytes, Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_e2e_test_utils::{
    setup, transaction::TransactionTestContext, wallet::Wallet,
};
use alloy_primitives::{TxKind, U256};
use alloy_sol_types::sol;
use reth_node_api::PayloadTypes;
use alloy_network::{EthereumWallet, TransactionBuilder};
use reth_optimism_chainspec::{OpChainSpecBuilder, OP_SEPOLIA};
use reth_optimism_node::{OpNode};
use reth_optimism_payload_builder::{OpPayloadBuilderAttributes};
use reth_optimism_primitives::OpTransactionSigned;
use reth_provider::BlockReaderIdExt;
use std::sync::Arc;
use alloy_eips::{BlockId, BlockNumberOrTag, Encodable2718};
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_revm::database::EvmStateProvider;
use reth_rpc_api::{EngineApiClient};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use alloy_rpc_types_eth::{BlockTransactions, TransactionRequest};
use alloy_rpc_types_eth::transaction::request::TransactionInput;
use reth_rpc_api::EthApiServer;
use alloy_sol_types::{SolCall, SolValue};

#[tokio::test]
async fn full_engine_api_bock_building_get_validation() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = OpChainSpecBuilder::default()
        .chain(OP_SEPOLIA.chain)
        .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
        .regolith_activated()
        .canyon_activated()
        .ecotone_activated()
        .build();

    let (mut nodes, _tasks, _wallet) = setup::<OpNode>(
        1,
        Arc::new(chain_spec.clone()),
        false,
        |timestamp| {
            let attributes = PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
            };

            // Construct Optimism-specific payload attributes
            OpPayloadBuilderAttributes::<OpTransactionSigned> {
                payload_attributes: EthPayloadBuilderAttributes::new(B256::ZERO, attributes),
                transactions: vec![], // Empty vector of transactions for the builder
                no_tx_pool: false,
                gas_limit: Some(30_000_000),
                eip_1559_params: None,
                min_base_fee: None,
            }
        },
    )
    .await?;

    let mut node = nodes.pop().unwrap();
    let provider = node.inner.provider.clone();

    let genesis_hash = node.block_hash(0);

    let wallet = Wallet::default();
    let raw_tx = TransactionTestContext::transfer_tx_bytes(OP_SEPOLIA.chain.id(), wallet.inner).await;
    let _tx_hash = node.rpc.inject_tx(raw_tx).await?;

    let current_head = provider.sealed_header_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)?.unwrap();
    let current_timestamp = current_head.timestamp;

    let payload_attrs = PayloadAttributes {
        timestamp: current_timestamp + 2, // 2 seconds after current block (OP block time)
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    let fcu_state = ForkchoiceState {
        head_block_hash: genesis_hash,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };

    let op_attrs = OpPayloadAttributes {
        payload_attributes: payload_attrs.clone(),
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };

    let engine_client = node.inner.engine_http_client();
    let fcu_result = engine_client
        .fork_choice_updated_v3(fcu_state, Some(op_attrs))
        .await?;
    let payload_id = fcu_result.payload_id.expect("payload id");

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let payload_v3 = engine_client.get_payload_v3(payload_id).await?;
    assert_eq!(genesis_hash, payload_v3.execution_payload.payload_inner.payload_inner.parent_hash);
    assert_eq!(21000, payload_v3.execution_payload.payload_inner.payload_inner.gas_used);

    // newPaylaod
    let payload_builder_handle = node.inner.payload_builder_handle.clone();
    let built_payload = payload_builder_handle
        .best_payload(payload_id)
        .await
        .transpose()
        .ok()
        .flatten()
        .expect("Payload should be built");
    let block = Arc::new(built_payload.block().clone());
    let payload_v3 = ExecutionPayloadV3::from_block_unchecked(
        block.hash(),
        &Arc::unwrap_or_clone(block.clone()).into_block(),
    );
    let versioned_hashes: Vec<B256> = Vec::new();
    let parent_beacon_block_root = block.parent_beacon_block_root.unwrap_or_default();

    let new_payload_result = engine_client
        .new_payload_v3(payload_v3, versioned_hashes, parent_beacon_block_root)
        .await?;
    assert_eq!(new_payload_result.status, PayloadStatusEnum::Valid);

    Ok(())
}


#[tokio::test]
async fn full_engine_api_bock_building_continuously() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = OpChainSpecBuilder::default()
        .chain(OP_SEPOLIA.chain)
        .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
        .regolith_activated()
        .canyon_activated()
        .ecotone_activated()
        .build();

    let (mut nodes, _tasks, _wallet) = setup::<OpNode>(
        1,
        Arc::new(chain_spec.clone()),
        false,
        |timestamp| {
            let attributes = PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
            };

            // Construct Optimism-specific payload attributes
            OpPayloadBuilderAttributes::<OpTransactionSigned> {
                payload_attributes: EthPayloadBuilderAttributes::new(B256::ZERO, attributes),
                transactions: vec![], // Empty vector of transactions for the builder
                no_tx_pool: false,
                gas_limit: Some(30_000_000),
                eip_1559_params: None,
                min_base_fee: None,
            }
        },
    )
        .await?;

    let mut node = nodes.pop().unwrap();
    let provider = node.inner.provider.clone();

    let genesis_hash = node.block_hash(0);

    let wallet = Wallet::default();
    let signer = wallet.inner.clone();
    let raw_tx = TransactionTestContext::transfer_tx_bytes(OP_SEPOLIA.chain.id(), signer.clone()).await;
    let _tx_hash = node.rpc.inject_tx(raw_tx).await?;

    let current_head = provider.sealed_header_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)?.unwrap();
    let current_timestamp = current_head.timestamp;

    let payload_attrs = PayloadAttributes {
        timestamp: current_timestamp + 2, // 2 seconds after current block (OP block time)
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    let fcu_state = ForkchoiceState {
        head_block_hash: genesis_hash,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };

    let op_attrs = OpPayloadAttributes {
        payload_attributes: payload_attrs.clone(),
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };

    let engine_client = node.inner.engine_http_client();
    let fcu_result = engine_client
        .fork_choice_updated_v3(fcu_state, Some(op_attrs))
        .await?;
    let payload_id = fcu_result.payload_id.expect("payload id");

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let payload_v3 = engine_client.get_payload_v3(payload_id).await?;
    let block_1_hash = payload_v3.execution_payload.payload_inner.payload_inner.block_hash;
    assert_eq!(genesis_hash, payload_v3.execution_payload.payload_inner.payload_inner.parent_hash);
    assert_eq!(21000, payload_v3.execution_payload.payload_inner.payload_inner.gas_used);

    // newPaylaod
    let payload_builder_handle = node.inner.payload_builder_handle.clone();
    let built_payload = payload_builder_handle
        .best_payload(payload_id)
        .await
        .transpose()
        .ok()
        .flatten()
        .expect("Payload should be built");
    let block = Arc::new(built_payload.block().clone());
    let payload_v3 = ExecutionPayloadV3::from_block_unchecked(
        block.hash(),
        &Arc::unwrap_or_clone(block.clone()).into_block(),
    );
    let versioned_hashes: Vec<B256> = Vec::new();
    let parent_beacon_block_root = block.parent_beacon_block_root.unwrap_or_default();

    let new_payload_result = engine_client
        .new_payload_v3(payload_v3, versioned_hashes, parent_beacon_block_root)
        .await?;
    assert_eq!(new_payload_result.status, PayloadStatusEnum::Valid);
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    // Build block2
    // let raw_tx2 = TransactionTestContext::transfer_tx_bytes(chain_spec.chain.id(), signer.clone()).await;
    let tx2 = TransactionRequest {
        nonce: Some(1),
        value: Some(U256::from(100)),
        to: Some(TxKind::Call(Address::random())),
        gas: Some(21000),
        max_fee_per_gas: Some(25e9 as u128),            // bump fee as needed
        max_priority_fee_per_gas: Some(20e9 as u128),
        chain_id: Some(chain_spec.chain.id()),
        ..Default::default()
    };
    let signed2 = TransactionTestContext::sign_tx(signer.clone(), tx2).await;
    let raw_tx2 = signed2.encoded_2718().into();
    let _tx_hash2 = node.rpc.inject_tx(raw_tx2).await?;
    let fcu_state_2 = ForkchoiceState {
        head_block_hash: block_1_hash,
        safe_block_hash: block_1_hash,
        finalized_block_hash: genesis_hash,
    };
    let payload_attrs_2 = PayloadAttributes {
        timestamp: payload_attrs.timestamp + 2,
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };
    let op_attrs_2 = OpPayloadAttributes {
        payload_attributes: payload_attrs_2.clone(),
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };
    let fcu_result_2 = engine_client
        .fork_choice_updated_v3(fcu_state_2, Some(op_attrs_2))
        .await?;
    assert_eq!(fcu_result_2.payload_status.status, PayloadStatusEnum::Valid);
    let payload_id_2 = fcu_result_2.payload_id.expect("second payload id");

    Ok(())
}

#[tokio::test]
async fn full_engine_api_multi_address() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = OpChainSpecBuilder::default()
        .chain(OP_SEPOLIA.chain)
        .genesis(serde_json::from_str(include_str!("../assets/genesis_token.json")).unwrap())
        .regolith_activated()
        .canyon_activated()
        .ecotone_activated()
        .build();

    let (mut nodes, _tasks, _wallet) = setup::<OpNode>(
        1,
        Arc::new(chain_spec.clone()),
        false,
        |timestamp| {
            let attributes = PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
            };

            // Construct Optimism-specific payload attributes
            OpPayloadBuilderAttributes::<OpTransactionSigned> {
                payload_attributes: EthPayloadBuilderAttributes::new(B256::ZERO, attributes),
                transactions: vec![], // Empty vector of transactions for the builder
                no_tx_pool: false,
                gas_limit: Some(30_000_000),
                eip_1559_params: None,
                min_base_fee: None,
            }
        },
    )
        .await?;

    let mut node = nodes.pop().unwrap();
    let provider = node.inner.provider.clone();
    let genesis_hash = node.block_hash(0);

    sol! {
       function balanceOf(address) view returns (uint256);
    }

    let token: Address = "0x5FbDB2315678afecb367f032d93F642f64180aa3".parse().unwrap();
    let receiver: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92265".parse().unwrap();

    let calldata = balanceOfCall(receiver).abi_encode();

    let call_req = TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::from(calldata), // not wrapped in Option
        ..Default::default()
    };
    let res_bytes = node
        .rpc
        .inner
        .eth_api()
        .call(call_req.clone().into(), Some(BlockId::latest()), None, None)
        .await?;

    if res_bytes.is_empty() {
        println!("Call returned empty bytes (revert or no code)");
    } else {
        let balance = balanceOfCall::abi_decode_returns(&res_bytes).expect("decode failed");
        println!("balanceOf receiver {receiver:x} = {balance}");
    }

    // let provider = node.inner.provider.clone();
    //
    // let genesis_hash = node.block_hash(0);
    //
    let wallet = Wallet::default();
    let sender_address = wallet.inner.address();


    let calldata: Bytes = hex!("a9059cbb000000000000000000000011f39Fd6e51aad88F6F4ce6aB8827279cffFb922650000000000000000000000000000000000000000000000000000000000000001").into();

    // Build tx
    let nonce = node.rpc.inner.eth_api().transaction_count(wallet.inner.address(), None).await.unwrap();
    let tx_request = TransactionRequest {
        from: Some(sender_address),
        to: Some(token.into()), // TxKind::Call
        value: Some(U256::ZERO),
        gas: Some(300_000), // sufficient gas
        gas_price: Some(1_000_000_000), // 1 gwei
        nonce: Some(nonce.to::<u64>()),
        input: calldata.into(), // TransactionInput
        chain_id: Some(OP_SEPOLIA.chain.id()),
        ..Default::default()
    };

    let signer = wallet.inner.clone();
    let wallet_wrapper = EthereumWallet::from(signer); // Wrap the signer
    let envelope = tx_request.build(&wallet_wrapper).await.unwrap();
    let raw_tx = envelope.encoded_2718();

    // Inject
    let tx_hash = node.rpc.inject_tx(raw_tx.into()).await?;
    println!("Injected transfer tx: {tx_hash}");

    let current_head = provider.sealed_header_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)?.unwrap();
    let current_timestamp = current_head.timestamp;

    let payload_attrs = PayloadAttributes {
        timestamp: current_timestamp + 2, // 2 seconds after current block (OP block time)
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    let fcu_state = ForkchoiceState {
        head_block_hash: genesis_hash,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };

    let op_attrs = OpPayloadAttributes {
        payload_attributes: payload_attrs.clone(),
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };

    let engine_client = node.inner.engine_http_client();
    let fcu_result = engine_client
        .fork_choice_updated_v3(fcu_state, Some(op_attrs))
        .await?;
    let payload_id = fcu_result.payload_id.expect("payload id");

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let payload_v3 = engine_client.get_payload_v3(payload_id).await?;
    assert_eq!(genesis_hash, payload_v3.execution_payload.payload_inner.payload_inner.parent_hash);
    // assert_eq!(68354, payload_v3.execution_payload.payload_inner.payload_inner.gas_used);

    // newPaylaod
    let payload_builder_handle = node.inner.payload_builder_handle.clone();
    let built_payload = payload_builder_handle
        .best_payload(payload_id)
        .await
        .transpose()
        .ok()
        .flatten()
        .expect("Payload should be built");
    let block = Arc::new(built_payload.block().clone());
    let payload2_v3 = ExecutionPayloadV3::from_block_unchecked(
        block.hash(),
        &Arc::unwrap_or_clone(block.clone()).into_block(),
    );
    let versioned_hashes: Vec<B256> = Vec::new();
    let parent_beacon_block_root = block.parent_beacon_block_root.unwrap_or_default();
    println!("payload2_v3: {:?}", payload2_v3);
    let new_payload_result = engine_client
        .new_payload_v3(payload2_v3.clone(), versioned_hashes, parent_beacon_block_root)
        .await?;
    assert_eq!(new_payload_result.status, PayloadStatusEnum::Valid);

    let head = payload_v3.execution_payload.payload_inner.payload_inner.block_hash;
    let fcu_state = ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: head,
        finalized_block_hash: head,
    };
    let ret = engine_client.fork_choice_updated_v3(fcu_state, None).await?;
    print!("ret: {:?}", ret);

    let latest_block = node
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Pending, false) // false = no full tx objects
        .await?
        .expect("latest block should exist");
    //
    println!("Unsafe/latest block number: {}", latest_block.header.number);
    //
    match &latest_block.transactions {
        BlockTransactions::Full(txs) => {
            for tx in txs {
                println!("Tx hash: {:?}", tx);
                // println!("Tx input: {:?}", tx.input);
            }
        }
        BlockTransactions::Hashes(hashes) => {
            println!("Block has {} tx hashes", hashes.len());
            // for h in hashes {
            //     if let Some(receipt) = node
            //         .rpc
            //         .inner
            //         .eth_api()
            //         .transaction_receipt(*h)
            //         .await?
            //     {
            //         println!("Tx {:?}", h);
            //     } else {
            //         println!("Tx {h:?} has no receipt yet");
            //     }
            // }
        }
        BlockTransactions::Uncle => {
            unreachable!()
        }
    }

    let res_bytes = node
        .rpc
        .inner
        .eth_api()
        .call(call_req.into(), Some(BlockId::latest()), None, None)
        .await?;

    if res_bytes.is_empty() {
        println!("Call returned empty bytes (revert or no code)");
    } else {
        let balance = balanceOfCall::abi_decode_returns(&res_bytes).expect("decode failed");
        println!("balanceOf receiver {receiver:x} = {balance}");
    }

    // for tx_bytes in &payload_v3.execution_payload.payload_inner.payload_inner.transactions {
    //     // Calculate hash of the transaction
    //     let hash = keccak256(tx_bytes);
    //
    //     // Get receipt
    //     let receipt = node
    //         .rpc
    //         .inner
    //         .eth_api()
    //         .transaction_receipt(hash)
    //         .await
    //         .expect("should not error")
    //         .expect("receipt should exist");
    //
    //     // Ensure success (status == 1)
    //     // assert!(receipt.inner.status_code.expect("status code").to_bool());
    //     println!("receipt: {:?}", receipt);
    // }


    Ok(())
}


#[test]
fn test_slot() {
    use alloy_primitives::{keccak256, Address, B256, U256};

    fn mapping_slot_balance_of(holder: Address) -> B256 {
        let slot = U256::from(1u64);

        // Left-pad the 20-byte address to 32 bytes
        let mut addr_word = [0u8; 32];
        addr_word[12..].copy_from_slice(holder.as_slice());

        // Build abi.encode(key, slot)
        let mut buf = [0u8; 64];
        buf[0..32].copy_from_slice(&addr_word);
        buf[32..64].copy_from_slice(&slot.to_be_bytes::<32>());

        keccak256(buf)
    }


        let holder: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse().unwrap();
        let storage_slot = mapping_slot_balance_of(holder);
        println!("{storage_slot:#x}");

}
