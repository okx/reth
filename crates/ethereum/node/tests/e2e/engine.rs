use crate::utils::eth_payload_attributes;
use alloy_genesis::Genesis;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{
    node::NodeTestContext, setup, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_node_ethereum::EthereumNode;
use reth_tasks::TaskManager;
use std::sync::Arc;
use reth_provider::BlockReaderIdExt;
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};

#[tokio::test]
async fn can_call_fcu_with_attributes_to_execute_next_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_sepc = ChainSpecBuilder::default()
        .chain(MAINNET.chain)
        .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
        .cancun_activated()
        .build();
    let (mut nodes, _tasks, _wallet) = setup::<EthereumNode>(
        1,
        Arc::new(chain_sepc.clone()),
        false,
        eth_payload_attributes,
    )
        .await?;

    let mut node = nodes.pop().unwrap();

    let genesis_hash = node.block_hash(0);

    let provider = &node.inner.provider;
    let current_head = provider
        .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
        .unwrap()
        .unwrap();
    let current_head_hash = current_head.hash();
    assert_eq!(current_head_hash,chain_sepc.genesis_hash());

    // let current_head_number = current_head.number();
    let current_timestamp = current_head.timestamp;
    //
    // Create payload attributes for the next block
    use alloy_rpc_types_engine::PayloadAttributes;
    use alloy_primitives::{Address, B256};
    use reth_payload_primitives::EngineApiMessageVersion;
    use reth_ethereum_engine_primitives::EthPayloadBuilderAttributes;

    let wallet = Wallet::default();
    let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner).await;
    let _tx_hash = node.rpc.inject_tx(raw_tx).await?;

    let payload_attrs = PayloadAttributes {
        timestamp: current_timestamp + 12, // 12 seconds after current block
        prev_randao: B256::random(),
        suggested_fee_recipient: Address::random(),
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };
    //
    // Call FCU with payload attributes
    use alloy_rpc_types_engine::ForkchoiceState;
    let fcu_state = ForkchoiceState {
        head_block_hash: current_head_hash,
        safe_block_hash: current_head_hash,
        finalized_block_hash: current_head_hash,
    };

    let fcu_result = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(
            fcu_state,
            Some(payload_attrs.into()),
            EngineApiMessageVersion::default(),
        )
        .await?;
    println!("fcu_result: {fcu_result:?}");

    let payload_id = fcu_result
        .payload_id
        .expect("FCU with attributes should return a payload ID");

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Get the built payload
    use reth_rpc_api::clients::EngineApiClient;
    // use reth_ethereum_engine_primitives::EthEngineTypes;

    let engine_client = node.inner.add_ons_handle.beacon_engine_handle.clone();
    // engine_client.
    // engine_client.new_payload().await;
    let payload_builder_handle = node.inner.payload_builder_handle.clone();

    // Wait a bit for payload to be built
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Get the best payload by payload_id
    let built_payload = payload_builder_handle
        .best_payload(payload_id)
        .await
        .transpose()
        .ok()
        .flatten()
        .expect("Payload should be built");

    // Convert the built payload to ExecutionData using the helper method
    use reth_ethereum_engine_primitives::EthEngineTypes;
    use reth_node_api::PayloadTypes;

    let execution_data = EthEngineTypes::<reth_ethereum_engine_primitives::EthPayloadTypes>::block_to_payload(
        built_payload.block().clone()
    );

    let new_payload_result = engine_client.new_payload(execution_data).await?;
    println!("new_payload_result: {new_payload_result:?}");


    // // Verify FCU was successful and got a payload ID
    // assert!(
    //     fcu_result.payload_status.is_valid(),
    //     "FCU should return valid status, got: {:?}",
    //     fcu_result.payload_status.status
    // );
    //
    // let payload_id = fcu_result
    //     .payload_id
    //     .expect("FCU with attributes should return a payload ID");
    //
    // // Wait a bit for payload to be built
    // tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    //
    // // Get the built payload
    // use reth_rpc_api::clients::EngineApiClient;
    // use reth_ethereum_engine_primitives::EthEngineTypes;
    //
    // let engine_client = node.inner.add_ons_handle.beacon_engine_handle.clone();
    // let payload_envelope = EngineApiClient::<EthEngineTypes>::get_payload_v3(
    //     &engine_client,
    //     payload_id,
    // )
    //     .await?;
    //
    // // Verify the payload
    // let built_block = payload_envelope.block();
    // assert_eq!(
    //     built_block.header.parent_hash,
    //     current_head_hash,
    //     "Built block should have correct parent hash"
    // );
    // assert_eq!(
    //     built_block.header.number,
    //     current_head_number + 1,
    //     "Built block should be next block number"
    // );
    // assert_eq!(
    //     built_block.header.timestamp,
    //     payload_attrs.timestamp,
    //     "Built block should have correct timestamp"
    // );
    //
    // // Submit the payload
    // let new_block_hash = node.submit_payload(payload_envelope.payload().clone()).await?;
    //
    // // Update forkchoice to make the new block canonical
    // node.update_forkchoice(current_head_hash, new_block_hash).await?;
    //
    // // Verify the new block is now the head
    // let new_head = provider
    //     .sealed_header_by_number_or_tag(alloy_eips::eip2718::BlockNumberOrTag::Latest)
    //     .unwrap()
    //     .unwrap();
    // assert_eq!(
    //     new_head.hash(),
    //     new_block_hash,
    //     "New block should be the canonical head"
    // );
    // assert_eq!(
    //     new_head.number(),
    //     current_head_number + 1,
    //     "New head should be next block number"
    // );

    Ok(())
}