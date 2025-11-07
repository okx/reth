//! Transaction trace arguments

use clap::Args;
use std::path::PathBuf;

/// Parameters for transaction tracing
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Transaction Trace")]
pub struct TransactionTraceArgs {
    /// Enable transaction tracing.
    ///
    /// When enabled, detailed transaction execution traces will be logged
    /// at each stage of the transaction lifecycle (RPC receive, pool add,
    /// execution, state application, etc.).
    #[arg(long = "tx-trace.enable", help_heading = "Transaction Trace")]
    pub enable: bool,

    /// Path to write transaction trace output file.
    ///
    /// If specified, all transaction traces will be written to a single file.
    /// Each log entry is written as a single JSON line, making it easy to parse.
    ///
    /// Path handling:
    /// - If the path ends with a directory separator (`/` or `\`), `trace.log` will be appended.
    /// - If the path has no extension and doesn't exist, it's treated as a directory and `trace.log` will be appended.
    /// - If the path has an extension (e.g., `.log`) or exists, it's used directly as the output file.
    ///
    /// Examples:
    /// - `--tx-trace.output-path=/datadir/logs` → writes to `/datadir/logs/trace.log`
    /// - `--tx-trace.output-path=/datadir/logs/my.log` → writes to `/datadir/logs/my.log`
    /// - `--tx-trace.output-path=/datadir/logs/` → writes to `/datadir/logs/trace.log`
    ///
    /// If not specified, traces will only be logged to the console.
    #[arg(
        long = "tx-trace.output-path",
        help_heading = "Transaction Trace",
        value_name = "PATH"
    )]
    pub output_path: Option<PathBuf>,
}

impl Default for TransactionTraceArgs {
    fn default() -> Self {
        Self {
            enable: false,
            output_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn transaction_trace_args_default_sanity_test() {
        let default_args = TransactionTraceArgs::default();
        let args = CommandParser::<TransactionTraceArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn transaction_trace_parse_enable() {
        let args = CommandParser::<TransactionTraceArgs>::parse_from([
            "reth",
            "--tx-trace.enable",
        ])
        .args;
        assert!(args.enable);
    }

    #[test]
    fn transaction_trace_parse_output_path() {
        let args = CommandParser::<TransactionTraceArgs>::parse_from([
            "reth",
            "--tx-trace.output-path",
            "/tmp/tx-traces",
        ])
        .args;
        assert_eq!(args.output_path, Some(PathBuf::from("/tmp/tx-traces")));
    }

    #[test]
    fn transaction_trace_parse_both() {
        let args = CommandParser::<TransactionTraceArgs>::parse_from([
            "reth",
            "--tx-trace.enable",
            "--tx-trace.output-path",
            "/tmp/tx-traces",
        ])
        .args;
        assert!(args.enable);
        assert_eq!(args.output_path, Some(PathBuf::from("/tmp/tx-traces")));
    }
}

