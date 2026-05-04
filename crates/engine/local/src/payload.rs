//! The implementation of the [`PayloadAttributesBuilder`] for the
//! [`LocalMiner`](super::LocalMiner).

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, B64};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_payload_primitives::PayloadAttributesBuilder;
use reth_primitives_traits::SealedHeader;
use std::sync::Arc;

/// The attributes builder for local Ethereum payload.
#[derive(Debug)]
#[non_exhaustive]
pub struct LocalPayloadAttributesBuilder<ChainSpec> {
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,

    /// Whether to enforce increasing timestamp.
    pub enforce_increasing_timestamp: bool,
}

impl<ChainSpec> LocalPayloadAttributesBuilder<ChainSpec> {
    /// Creates a new instance of the builder.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec, enforce_increasing_timestamp: true }
    }

    /// Creates a new instance of the builder without enforcing increasing timestamps.
    pub fn without_increasing_timestamp(self) -> Self {
        Self { enforce_increasing_timestamp: false, ..self }
    }
}

impl<ChainSpec> PayloadAttributesBuilder<EthPayloadAttributes, ChainSpec::Header>
    for LocalPayloadAttributesBuilder<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks + 'static,
{
    fn build(&self, parent: &SealedHeader<ChainSpec::Header>) -> EthPayloadAttributes {
        let mut timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        if self.enforce_increasing_timestamp {
            timestamp = std::cmp::max(parent.timestamp().saturating_add(1), timestamp);
        }

        EthPayloadAttributes {
            timestamp,
            prev_randao: B256::random(),
            suggested_fee_recipient: Address::random(),
            withdrawals: self
                .chain_spec
                .is_shanghai_active_at_timestamp(timestamp)
                .then(Default::default),
            parent_beacon_block_root: self
                .chain_spec
                .is_cancun_active_at_timestamp(timestamp)
                .then(B256::random),
            slot_number: self.chain_spec.is_amsterdam_active_at_timestamp(timestamp).then_some(0),
        }
    }
}

#[cfg(feature = "op")]
impl<ChainSpec>
    PayloadAttributesBuilder<op_alloy_rpc_types_engine::OpPayloadAttributes, ChainSpec::Header>
    for LocalPayloadAttributesBuilder<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks + reth_optimism_chainspec::OpHardforks + 'static,
{
    fn build(
        &self,
        parent: &SealedHeader<ChainSpec::Header>,
    ) -> op_alloy_rpc_types_engine::OpPayloadAttributes {
        let eth_attrs: EthPayloadAttributes = self.build(parent);
        let timestamp = eth_attrs.timestamp;

        let is_holocene = self.chain_spec.is_holocene_active_at_timestamp(timestamp);
        let is_jovian = self.chain_spec.is_jovian_active_at_timestamp(timestamp);

        // After Holocene, eip_1559_params must be set (B64::ZERO = use chain defaults).
        let eip_1559_params = is_holocene.then_some(B64::ZERO);
        // After Jovian, min_base_fee must be set in payload attributes.
        let min_base_fee = is_jovian.then_some(0u64);

        op_alloy_rpc_types_engine::OpPayloadAttributes {
            payload_attributes: eth_attrs,
            // Add dummy system transaction
            transactions: Some(vec![
                reth_optimism_chainspec::constants::TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056
                    .into(),
            ]),
            no_tx_pool: None,
            gas_limit: None,
            eip_1559_params,
            min_base_fee,
        }
    }
}
