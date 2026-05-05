//! XLayer Mainnet chain specification

use alloc::sync::Arc;
use alloy_chains::Chain;
use alloy_primitives::{b256, B256, U256};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
use reth_primitives_traits::{sync::LazyLock, SealedHeader};

use crate::{make_genesis_header, BaseFeeParams, BaseFeeParamsKind, ChainSpec};

/// X Layer Mainnet genesis hash
///
/// Computed from the genesis block header.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_MAINNET_GENESIS_HASH: B256 =
    b256!("dc33d8c0ec9de14fc2c21bd6077309a0a856df22821bd092a2513426e096a789");

/// X Layer Mainnet genesis state root
///
/// The Merkle Patricia Trie root of all accounts in the genesis alloc.
/// This value is hardcoded to avoid expensive computation on every startup.
pub(crate) const XLAYER_MAINNET_STATE_ROOT: B256 =
    b256!("5d335834cb1c1c20a1f44f964b16cd409aa5d10891d5c6cf26f1f2c26726efcf");

/// X Layer mainnet EIP-1559 base fee parameters.
///
/// These values come from `config.optimism` in `genesis-mainnet.json`:
/// - `eip1559Denominator = 100000000`
/// - `eip1559Elasticity = 1`
const XLAYER_MAINNET_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams::new(100_000_000, 1);

/// The X Layer mainnet spec
pub static XLAYER_MAINNET: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(include_str!("../res/genesis/xlayer-mainnet.json"))
        .expect("Can't deserialize XLayer Mainnet genesis json");
    let hardforks = ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
        (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
        (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)),
    ]);

    // Build genesis header from JSON fields; override state_root with the pre-computed value
    // since the minimal genesis has an empty alloc (the full alloc would be expensive to hash).
    let mut genesis_header = make_genesis_header(&genesis, &hardforks);
    genesis_header.state_root = XLAYER_MAINNET_STATE_ROOT;

    ChainSpec {
        chain: Chain::from_id(196),
        genesis_header: SealedHeader::new(genesis_header, XLAYER_MAINNET_GENESIS_HASH),
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::ZERO)),
        hardforks,
        base_fee_params: BaseFeeParamsKind::Constant(XLAYER_MAINNET_BASE_FEE_PARAMS),
        ..Default::default()
    }
    .into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use reth_ethereum_forks::EthereumHardforks;

    #[test]
    fn test_xlayer_mainnet_chain_id() {
        assert_eq!(XLAYER_MAINNET.chain().id(), 196);
    }

    #[test]
    fn test_xlayer_mainnet_genesis_hash() {
        assert_eq!(XLAYER_MAINNET.genesis_hash(), XLAYER_MAINNET_GENESIS_HASH);
    }

    #[test]
    fn test_xlayer_mainnet_state_root() {
        assert_eq!(XLAYER_MAINNET.genesis_header().state_root, XLAYER_MAINNET_STATE_ROOT);
    }

    #[test]
    fn test_xlayer_mainnet_genesis_number() {
        assert_eq!(XLAYER_MAINNET.genesis_header().number, 42810021);
    }

    #[test]
    fn test_xlayer_mainnet_hardforks() {
        assert!(XLAYER_MAINNET.is_shanghai_active_at_timestamp(0));
        assert!(XLAYER_MAINNET.is_cancun_active_at_timestamp(0));
    }
}
