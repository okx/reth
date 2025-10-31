use clap::Args;

/// Parameters to configure Inner Tx
#[derive(Debug, Args, PartialEq, Eq, Default, Clone, Copy)]
#[command(next_help_heading = "InnerTx")]
pub struct InnerTxArgs {
    /// Enable capturing of inner txns during evm execution
    ///
    /// If true, a custom inspector will be hooked onto
    /// `EthereumExecutorBuilder` to inspect for inner txn
    /// fields.
    ///
    /// [default: false]
    #[arg(long = "innertx.enabled", default_value_t = false)]
    pub capture_enabled: bool,
}
