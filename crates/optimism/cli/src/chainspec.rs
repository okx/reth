use reth_cli::chainspec::{parse_genesis, ChainSpecParser};
use reth_optimism_chainspec::{generated_chain_value_parser, OpChainSpec, SUPPORTED_CHAINS};
use std::sync::Arc;

/// Optimism chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpChainSpecParser;

impl ChainSpecParser for OpChainSpecParser {
    type ChainSpec = OpChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// Clap value parser for [`OpChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<OpChainSpec>, eyre::Error> {
    use tracing::info;

    if let Some(op_chain_spec) = generated_chain_value_parser(s) {
        info!(target: "reth::cli::genesis", chain = %s, "Using built-in chain spec");
        Ok(op_chain_spec)
    } else {
        let start = std::time::Instant::now();
        info!(target: "reth::cli::genesis", path = %s, "Parsing custom OpChainSpec from genesis file");

        let genesis = parse_genesis(s)?;

        let convert_start = std::time::Instant::now();
        let chain_spec = Arc::new(genesis.into());
        let convert_elapsed = convert_start.elapsed();

        info!(
            target: "reth::cli::genesis",
            convert_elapsed_ms = convert_elapsed.as_millis(),
            total_elapsed_ms = start.elapsed().as_millis(),
            "OpChainSpec conversion completed"
        );

        Ok(chain_spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in OpChainSpecParser::SUPPORTED_CHAINS {
            assert!(
                <OpChainSpecParser as ChainSpecParser>::parse(chain).is_ok(),
                "Failed to parse {chain}"
            );
        }
    }
}
