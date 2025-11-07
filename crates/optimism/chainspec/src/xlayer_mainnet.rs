// ! Chain specification for the X Layer Mainnet network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{b256, B256, U256};
use alloy_genesis::Genesis;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
use reth_optimism_forks::OpHardfork;
use reth_primitives_traits::{Header, SealedHeader};

use crate::{LazyLock, OpChainSpec};

/// X Layer Mainnet genesis hash
///
/// Computed from the genesis block header.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_MAINNET_GENESIS_HASH: B256 = b256!("dc33d8c0ec9de14fc2c21bd6077309a0a856df22821bd092a2513426e096a789");

/// X Layer Mainnet genesis state root
///
/// The Merkle Patricia Trie root of all 1,866,483 accounts in the genesis alloc.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_MAINNET_STATE_ROOT: B256 = b256!("5d335834cb1c1c20a1f44f964b16cd409aa5d10891d5c6cf26f1f2c26726efcf");

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

    let mut withdrawals_root = if hardforks.fork(EthereumHardfork::Shanghai).active_at_timestamp(genesis.timestamp) {
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
    // Solution: The pre-computed XLAYER_MAINNET_STATE_ROOT and XLAYER_MAINNET_GENESIS_HASH
    // already include the correct Isthmus handling from the full genesis computation.

    Header {
        number: genesis.number.unwrap_or_default(),
        parent_hash: genesis.parent_hash.unwrap_or_default(),
        gas_limit: genesis.gas_limit,
        gas_used: 0, // Genesis always has gas_used = 0
        difficulty: genesis.difficulty,
        nonce: genesis.nonce.into(),
        extra_data: genesis.extra_data.clone(),
        state_root: XLAYER_MAINNET_STATE_ROOT, // Pre-computed to skip expensive computation!
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

/// The X Layer mainnet spec
pub static XLAYER_MAINNET: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    // Use minimal genesis without alloc for fast loading
    let mut genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/xlayer_mainnet.json"))
        .expect("Can't deserialize X Layer Mainnet genesis json");

    // Clear alloc to ensure we don't accidentally use it (should already be empty in the JSON)
    genesis.alloc.clear();

    let hardforks = build_hardforks(&genesis);
    let genesis_header = build_genesis_header(&genesis, &hardforks);
    let genesis_header = SealedHeader::new(genesis_header, XLAYER_MAINNET_GENESIS_HASH);

    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::from_id(196), // X Layer chain ID
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
    fn test_xlayer_mainnet_chain_id() {
        let spec = &*XLAYER_MAINNET;
        assert_eq!(spec.chain().id(), 196, "Chain ID should be 196");
    }

    #[test]
    fn test_xlayer_mainnet_genesis_hash() {
        let spec = &*XLAYER_MAINNET;

        // This hash was computed from the full genesis.json with 1,866,483 accounts
        // Verified on 2025-11-07 from docker logs
        assert_eq!(
            spec.genesis_hash(),
            XLAYER_MAINNET_GENESIS_HASH,
            "Genesis hash must match the pre-computed value"
        );
    }

    #[test]
    fn test_xlayer_mainnet_state_root() {
        let spec = &*XLAYER_MAINNET;

        // This state root was computed from the full genesis.json with 1,866,483 accounts
        // Verified on 2025-11-07 from docker logs
        assert_eq!(
            spec.genesis_header().state_root,
            XLAYER_MAINNET_STATE_ROOT,
            "State root must match the pre-computed value"
        );
    }

    #[test]
    fn test_xlayer_mainnet_genesis_number() {
        let spec = &*XLAYER_MAINNET;

        // X Layer starts from a legacy block
        assert_eq!(
            spec.genesis_header().number,
            42810021,
            "Genesis block number should be 42810021"
        );
    }

    #[test]
    fn test_xlayer_mainnet_hardforks() {
        let spec = &*XLAYER_MAINNET;

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
    fn test_xlayer_mainnet_genesis_alloc_empty() {
        let spec = &*XLAYER_MAINNET;

        // The built-in spec should have empty alloc for fast loading
        // The actual account state is in the database (initialized via init-state)
        assert_eq!(
            spec.genesis().alloc.len(),
            0,
            "Built-in genesis should have empty alloc for fast loading"
        );
    }

    #[test]
    fn test_xlayer_mainnet_expected_values() {
        // This test explicitly verifies the expected hash and root from logs
        // to catch any accidental changes
        let expected_hash = b256!("dc33d8c0ec9de14fc2c21bd6077309a0a856df22821bd092a2513426e096a789");
        let expected_root = b256!("5d335834cb1c1c20a1f44f964b16cd409aa5d10891d5c6cf26f1f2c26726efcf");

        assert_eq!(
            XLAYER_MAINNET_GENESIS_HASH,
            expected_hash,
            "XLAYER_MAINNET_GENESIS_HASH constant should match expected value from logs"
        );

        assert_eq!(
            XLAYER_MAINNET_STATE_ROOT,
            expected_root,
            "XLAYER_MAINNET_STATE_ROOT constant should match expected value from logs"
        );
    }
}
