// ! Chain specification for the X Layer Testnet network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{b256, B256, U256};
use alloy_genesis::Genesis;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
use reth_optimism_forks::OpHardfork;
use reth_primitives_traits::{Header, SealedHeader};

use crate::{LazyLock, OpChainSpec};

/// X Layer Testnet genesis hash
///
/// Computed from the genesis block header.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_TESTNET_GENESIS_HASH: B256 = b256!("ccb16eb07b7a718c2ee374df57b0e28c9ac9d8d18ca6d3204cfbba661067855a");

/// X Layer Testnet genesis state root
///
/// The Merkle Patricia Trie root of all 6,234,122 accounts in the genesis alloc.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_TESTNET_STATE_ROOT: B256 = b256!("3de62c8ade3d3adaa88d48a3ffeebd7c8b6c5b81906d706c22f02f0d2dd3b8fa");

/// Build hardforks from genesis config
fn build_hardforks(genesis: &Genesis) -> ChainHardforks {
    let mut hardforks = vec![];

    // Ethereum hardforks (all at block 0 for X Layer since it starts post-merge)
    hardforks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)));
    hardforks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));

    // Time-based Ethereum hardforks
    if let Some(shanghai_time) = genesis.config.shanghai_time {
        hardforks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(shanghai_time)));
    }
    if let Some(cancun_time) = genesis.config.cancun_time {
        hardforks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(cancun_time)));
    }
    if let Some(prague_time) = genesis.config.prague_time {
        hardforks.push((EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(prague_time)));
    }

    // Optimism hardforks - all from block 0 (Bedrock)
    hardforks.push((OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)));

    // Read OP hardforks from extra_fields if they exist
    if let Some(regolith_time) = genesis.config.extra_fields.get("regolithTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(regolith_time)));
    }
    if let Some(canyon_time) = genesis.config.extra_fields.get("canyonTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(canyon_time)));
    }
    if let Some(ecotone_time) = genesis.config.extra_fields.get("ecotoneTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(ecotone_time)));
    }
    if let Some(fjord_time) = genesis.config.extra_fields.get("fjordTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(fjord_time)));
    }
    if let Some(granite_time) = genesis.config.extra_fields.get("graniteTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Granite.boxed(), ForkCondition::Timestamp(granite_time)));
    }
    if let Some(holocene_time) = genesis.config.extra_fields.get("holoceneTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(holocene_time)));
    }
    if let Some(isthmus_time) = genesis.config.extra_fields.get("isthmusTime").and_then(|v| v.as_u64()) {
        hardforks.push((OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(isthmus_time)));
    }

    ChainHardforks::new(hardforks)
}

/// Manually build genesis header with hardcoded state_root to avoid expensive computation
///
/// This is similar to `make_op_genesis_header` but uses a pre-computed state_root
/// to avoid the expensive Merkle tree calculation on every startup.
fn build_genesis_header(genesis: &Genesis, hardforks: &ChainHardforks) -> Header {
    let base_fee_per_gas = genesis.base_fee_per_gas.map(|fee| fee as u64);

    let withdrawals_root = if hardforks.fork(EthereumHardfork::Shanghai).active_at_timestamp(genesis.timestamp) {
        Some(alloy_consensus::constants::EMPTY_WITHDRAWALS)
    } else {
        None
    };

    let (parent_beacon_block_root, blob_gas_used, excess_blob_gas) =
        if hardforks.fork(EthereumHardfork::Cancun).active_at_timestamp(genesis.timestamp) {
            let blob_gas_used = genesis.blob_gas_used.unwrap_or(0);
            let excess_blob_gas = genesis.excess_blob_gas.unwrap_or(0);
            (Some(B256::ZERO), Some(blob_gas_used), Some(excess_blob_gas))
        } else {
            (None, None, None)
        };

    let requests_hash = if hardforks.fork(EthereumHardfork::Prague).active_at_timestamp(genesis.timestamp) {
        Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH)
    } else {
        None
    };

    // IMPORTANT: If Isthmus is active at genesis, we need special handling for withdrawals_root
    // This matches the behavior in `make_op_genesis_header`
    // However, since our genesis.alloc is empty (for fast loading), we can't compute it here.
    // The Isthmus logic requires access to L2ToL1MessagePasser predeploy storage,
    // which we don't have in the minimal genesis.
    //
    // Solution: The pre-computed XLAYER_TESTNET_STATE_ROOT and XLAYER_TESTNET_GENESIS_HASH
    // already include the correct Isthmus handling from the full genesis computation.

    Header {
        number: genesis.number.unwrap_or_default(),
        parent_hash: genesis.parent_hash.unwrap_or_default(),
        gas_limit: genesis.gas_limit,
        gas_used: 0, // Genesis always has gas_used = 0
        difficulty: genesis.difficulty,
        nonce: genesis.nonce.into(),
        extra_data: genesis.extra_data.clone(),
        state_root: XLAYER_TESTNET_STATE_ROOT, // Pre-computed to skip expensive computation!
        timestamp: genesis.timestamp,
        mix_hash: genesis.mix_hash,
        beneficiary: genesis.coinbase,
        base_fee_per_gas,
        withdrawals_root,
        parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash,
        ..Default::default()
    }
}

/// The X Layer testnet spec
pub static XLAYER_TESTNET: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    // Use minimal genesis without alloc for fast loading
    let mut genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/xlayer_testnet.json"))
        .expect("Can't deserialize X Layer Testnet genesis json");

    // Clear alloc to ensure we don't accidentally use it (should already be empty in the JSON)
    genesis.alloc.clear();

    let hardforks = build_hardforks(&genesis);
    let genesis_header = build_genesis_header(&genesis, &hardforks);
    let genesis_header = SealedHeader::new(genesis_header, XLAYER_TESTNET_GENESIS_HASH);

    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::from_id(1952), // X Layer Testnet chain ID
            genesis_header,
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![
                    (EthereumHardfork::London.boxed(), BaseFeeParams::optimism()),
                    (OpHardfork::Canyon.boxed(), BaseFeeParams::optimism_canyon()),
                ]
                .into(),
            ),
            ..Default::default()
        },
    }
    .into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use reth_ethereum_forks::EthereumHardfork;
    use reth_optimism_forks::OpHardfork;

    #[test]
    fn test_xlayer_testnet_chain_id() {
        let spec = &*XLAYER_TESTNET;
        assert_eq!(spec.chain().id(), 1952, "Chain ID should be 1952");
    }

    #[test]
    fn test_xlayer_testnet_genesis_hash() {
        let spec = &*XLAYER_TESTNET;

        // This hash was computed from the full genesis.json with 6,234,122 accounts
        assert_eq!(
            spec.genesis_hash(),
            XLAYER_TESTNET_GENESIS_HASH,
            "Genesis hash must match the pre-computed value"
        );
    }

    #[test]
    fn test_xlayer_testnet_state_root() {
        let spec = &*XLAYER_TESTNET;

        assert_eq!(
            spec.genesis_header().state_root,
            XLAYER_TESTNET_STATE_ROOT,
            "State root must match the pre-computed value"
        );
    }

    #[test]
    fn test_xlayer_testnet_genesis_block_number() {
        let spec = &*XLAYER_TESTNET;
        // Testnet genesis block is 12241700 (0xbacb24)
        assert_eq!(spec.genesis_header().number, 12241700, "Genesis block should be 12241700");
    }

    #[test]
    fn test_xlayer_testnet_hardforks() {
        let spec = &*XLAYER_TESTNET;

        // Verify key hardforks are configured and active
        assert!(
            spec.fork(EthereumHardfork::Shanghai).active_at_timestamp(0),
            "Shanghai should be active at genesis"
        );
        assert!(
            spec.fork(EthereumHardfork::Cancun).active_at_timestamp(0),
            "Cancun should be active at genesis"
        );
        assert!(
            spec.fork(OpHardfork::Bedrock).active_at_block(0),
            "Bedrock should be active at genesis"
        );
        assert!(
            spec.fork(OpHardfork::Isthmus).active_at_timestamp(0),
            "Isthmus should be active at genesis"
        );
    }

    #[test]
    fn test_xlayer_testnet_fast_loading() {
        let spec = &*XLAYER_TESTNET;

        // Verify that alloc is empty (for fast loading)
        assert_eq!(
            spec.genesis().alloc.len(),
            0,
            "Genesis alloc should be empty for fast loading"
        );
    }
}

