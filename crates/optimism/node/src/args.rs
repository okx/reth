//! Additional Node command arguments.

//! clap [Args](clap::Args) for optimism rollup configuration

use op_alloy_consensus::interop::SafetyLevel;
use reth_optimism_txpool::supervisor::DEFAULT_SUPERVISOR_URL;
use url::Url;

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct RollupArgs {
    /// Endpoint for the sequencer mempool (can be both HTTP and WS)
    #[arg(long = "rollup.sequencer", visible_aliases = ["rollup.sequencer-http", "rollup.sequencer-ws"])]
    pub sequencer: Option<String>,

    /// Disable transaction pool gossip
    #[arg(long = "rollup.disable-tx-pool-gossip")]
    pub disable_txpool_gossip: bool,

    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    #[arg(long = "rollup.compute-pending-block")]
    pub compute_pending_block: bool,

    /// enables discovery v4 if provided
    #[arg(long = "rollup.discovery.v4", default_value = "false")]
    pub discovery_v4: bool,

    /// Enable transaction conditional support on sequencer
    #[arg(long = "rollup.enable-tx-conditional", default_value = "false")]
    pub enable_tx_conditional: bool,

    /// Enable gasless transactions in the mempool.
    ///
    /// When enabled, transactions with a zero gas price (legacy `gas_price == 0`, or EIP-1559
    /// `max_fee_per_gas == 0 && max_priority_fee_per_gas == 0`) are accepted into the pool and
    /// gated by the on-chain gasless whitelist contract.
    #[arg(long = "rollup.allow-gasless", default_value = "false")]
    pub allow_gasless: bool,

    /// Percentile of the previous block's transaction gas prices used as the mock gas price
    /// assigned to gasless transactions for pool ordering. Accepts a fraction in `0.0..=1.0`
    /// (e.g. `0.1` ≈ low, `0.9` ≈ high); stored as basis points (`0..=10000`) so that
    /// [`RollupArgs`] stays `Eq`.
    #[arg(
        long = "rollup.gasless-mock-gas-price-percentile",
        default_value = "0.1",
        value_parser = parse_gasless_percentile_bps
    )]
    pub gasless_mock_gas_price_percentile_bps: u16,

    /// HTTP endpoint for the supervisor
    #[arg(
        long = "rollup.supervisor-http",
        value_name = "SUPERVISOR_HTTP_URL",
        default_value = DEFAULT_SUPERVISOR_URL
    )]
    pub supervisor_http: String,

    /// Safety level for the supervisor
    #[arg(
        long = "rollup.supervisor-safety-level",
        default_value_t = SafetyLevel::CrossUnsafe,
    )]
    pub supervisor_safety_level: SafetyLevel,

    /// Optional headers to use when connecting to the sequencer.
    #[arg(long = "rollup.sequencer-headers", requires = "sequencer")]
    pub sequencer_headers: Vec<String>,

    /// RPC endpoint for historical data.
    #[arg(
        long = "rollup.historicalrpc",
        alias = "rollup.historical-rpc",
        value_name = "HISTORICAL_HTTP_URL"
    )]
    pub historical_rpc: Option<String>,

    /// Minimum suggested priority fee (tip) in wei, default `1_000_000`
    #[arg(long, default_value_t = 1_000_000)]
    pub min_suggested_priority_fee: u64,

    /// A URL pointing to a secure websocket subscription that streams out flashblocks.
    ///
    /// If given, the flashblocks are received to build pending block. All request with "pending"
    /// block tag will use the pending state based on flashblocks.
    #[arg(long, alias = "websocket-url")]
    pub flashblocks_url: Option<Url>,

    /// Enable flashblock consensus client to drive the chain forward
    ///
    /// When enabled, the flashblock consensus client will process flashblock sequences and submit
    /// them to the engine API to advance the chain.
    /// Requires `flashblocks_url` to be set.
    #[arg(long, default_value_t = false, requires = "flashblocks_url")]
    pub flashblock_consensus: bool,
}

/// Default percentile (fraction in `0.0..=1.0`) of the previous block's paid gas prices used as the
/// gasless mock gas price. Single source of truth for the CLI default, the [`RollupArgs`] `Default`
/// impl, and `OpPoolBuilder`'s default — the CLI string default below is kept consistent with this
/// by `test_parse_optimism_default_args`.
pub const GASLESS_DEFAULT_MOCK_PRICE_PERCENTILE: f64 = 0.1;

/// Converts a `[0.0, 1.0]` percentile fraction into basis points (`0..=10000`).
pub(crate) fn percentile_to_bps(value: f64) -> u16 {
    (value.clamp(0.0, 1.0) * 10_000.0).round() as u16
}

/// Parses a `[0.0, 1.0]` percentile fraction into basis points (`0..=10000`).
fn parse_gasless_percentile_bps(s: &str) -> Result<u16, String> {
    let value: f64 = s.parse().map_err(|e| format!("invalid percentile `{s}`: {e}"))?;
    if !(0.0..=1.0).contains(&value) {
        return Err(format!("percentile must be in [0.0, 1.0], got {value}"));
    }
    Ok(percentile_to_bps(value))
}

impl Default for RollupArgs {
    fn default() -> Self {
        Self {
            sequencer: None,
            disable_txpool_gossip: false,
            compute_pending_block: false,
            discovery_v4: false,
            enable_tx_conditional: false,
            allow_gasless: false,
            gasless_mock_gas_price_percentile_bps: percentile_to_bps(
                GASLESS_DEFAULT_MOCK_PRICE_PERCENTILE,
            ),
            supervisor_http: DEFAULT_SUPERVISOR_URL.to_string(),
            supervisor_safety_level: SafetyLevel::CrossUnsafe,
            sequencer_headers: Vec::new(),
            historical_rpc: None,
            min_suggested_priority_fee: 1_000_000,
            flashblocks_url: None,
            flashblock_consensus: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Args, Parser};

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_optimism_default_args() {
        let default_args = RollupArgs::default();
        let args = CommandParser::<RollupArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn test_parse_optimism_compute_pending_block_args() {
        let expected_args = RollupArgs { compute_pending_block: true, ..Default::default() };
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.compute-pending-block"])
                .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_discovery_v4_args() {
        let expected_args = RollupArgs { discovery_v4: true, ..Default::default() };
        let args = CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.discovery.v4"]).args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_sequencer_http_args() {
        let expected_args =
            RollupArgs { sequencer: Some("http://host:port".into()), ..Default::default() };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.sequencer-http",
            "http://host:port",
        ])
        .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_disable_txpool_args() {
        let expected_args = RollupArgs { disable_txpool_gossip: true, ..Default::default() };
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.disable-tx-pool-gossip"])
                .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_enable_tx_conditional() {
        let expected_args = RollupArgs { enable_tx_conditional: true, ..Default::default() };
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.enable-tx-conditional"])
                .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_many_args() {
        let expected_args = RollupArgs {
            disable_txpool_gossip: true,
            compute_pending_block: true,
            enable_tx_conditional: true,
            sequencer: Some("http://host:port".into()),
            ..Default::default()
        };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.disable-tx-pool-gossip",
            "--rollup.compute-pending-block",
            "--rollup.enable-tx-conditional",
            "--rollup.sequencer-http",
            "http://host:port",
        ])
        .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_optimism_allow_gasless() {
        let expected_args = RollupArgs {
            allow_gasless: true,
            gasless_mock_gas_price_percentile_bps: 9000,
            ..Default::default()
        };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.allow-gasless",
            "--rollup.gasless-mock-gas-price-percentile",
            "0.9",
        ])
        .args;
        assert_eq!(args, expected_args);
    }
}
