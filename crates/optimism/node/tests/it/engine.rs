use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_e2e_test_utils::{
    setup, transaction::TransactionTestContext, wallet::Wallet,
};
// use reth_ethereum_engine_primitives::EthPayloadBuilderAttributes;
use reth_node_api::PayloadTypes;
use reth_optimism_chainspec::{OpChainSpecBuilder, OP_MAINNET, OP_SEPOLIA};
use reth_optimism_node::{OpEngineTypes, OpNode};
use reth_optimism_payload_builder::{OpPayloadTypes,OpPayloadBuilderAttributes};
use reth_optimism_primitives::OpTransactionSigned;
use reth_provider::BlockReaderIdExt;
use std::sync::Arc;
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_payload_primitives::EngineApiMessageVersion;

#[tokio::test]
async fn can_call_fcu_with_attributes_to_execute_next_block() -> eyre::Result<()> {
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

    // Create a wallet from genesis account and add a transaction to the txpool
    let wallet = Wallet::default();
    let raw_tx = TransactionTestContext::transfer_tx_bytes(OP_SEPOLIA.chain.id(), wallet.inner).await;
    let _tx_hash = node.rpc.inject_tx(raw_tx).await?;

    // Create payload attributes for the next block
    let current_head = provider.sealed_header_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)?.unwrap();
    let current_timestamp = current_head.timestamp;

    let payload_attrs = PayloadAttributes {
        timestamp: current_timestamp + 2, // 2 seconds after current block (OP block time)
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    // Call FCU with payload attributes
    let fcu_state = ForkchoiceState {
        head_block_hash: genesis_hash,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };

    // Wrap in OpPayloadAttributes
    let op_attrs = OpPayloadAttributes {
        payload_attributes: payload_attrs.clone(),
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    };

    let engine_api = node.inner.add_ons_handle.beacon_engine_handle.clone();

    // Use V3 because we included parent_beacon_block_root (Ecotone)
    let fcu_result = engine_api
        .fork_choice_updated(
            fcu_state,
            Some(op_attrs),
            EngineApiMessageVersion::V3,
        )
        .await?;

    assert_eq!(fcu_result.payload_status.status, PayloadStatusEnum::Valid);
    let payload_id = fcu_result
        .payload_id
        .expect("FCU with attributes should return a payload ID");

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Get the built payload from the builder service
    let payload_builder_handle = node.inner.payload_builder_handle.clone();
    let built_payload = payload_builder_handle
        .best_payload(payload_id)
        .await
        .transpose()
        .ok()
        .flatten()
        .expect("Payload should be built");

    // Convert to ExecutionData for NewPayload
    let execution_data =  OpEngineTypes::<OpPayloadTypes>::block_to_payload(built_payload.block().clone());

    // Submit the payload via Engine API
    let new_payload_result = engine_api.new_payload(execution_data).await?;
    assert_eq!(new_payload_result.status, PayloadStatusEnum::Valid);
    
    let new_block_hash = built_payload.block().hash();

    // Update forkchoice to make the new block canonical
    engine_api.fork_choice_updated(
        ForkchoiceState {
            head_block_hash: new_block_hash,
            safe_block_hash: new_block_hash,
            finalized_block_hash: genesis_hash,
        },
        None,
        EngineApiMessageVersion::V3,
    ).await?;

    // Verify the new block is now the head
    let new_head = provider.sealed_header_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)?.unwrap();
    assert_eq!(new_head.number, 1);
    assert_eq!(new_head.hash(), new_block_hash);

    Ok(())
}
