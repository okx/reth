//! CLI tool to profile X Layer blocks and measure real trie update sizes
//!
//! Usage:
//!   cargo run --bin xlayer-trie-profiler -- --rpc-url <url> --start-block <num> --block-count <count>
//!
//! Example:
//!   cargo run --bin xlayer-trie-profiler -- --rpc-url http://localhost:8545 --start-block 1000 --block-count 100

use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Client, Http};
use clap::Parser;
use reth_chain_state::{AggregatedTrieStats, BlockTrieStats};
use reth_primitives_traits::NodePrimitives;
use reth_provider::{BlockNumReader, BlockReader, ProviderFactory};
use reth_trie::updates::TrieUpdates;
use std::{path::PathBuf, sync::Arc};

#[derive(Parser, Debug)]
#[command(author, version, about = "Profile X Layer blocks to measure trie update sizes", long_about = None)]
struct Args {
    /// Database path (for local profiling)
    #[arg(long, value_name = "PATH")]
    datadir: Option<PathBuf>,

    /// Starting block number to analyze
    #[arg(long, default_value = "1000")]
    start_block: u64,

    /// Number of blocks to analyze
    #[arg(long, default_value = "100")]
    block_count: usize,

    /// Output detailed per-block stats
    #[arg(long)]
    verbose: bool,

    /// Export results to JSON file
    #[arg(long, value_name = "PATH")]
    export: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║         X Layer Trie Profiler - Performance Analysis          ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    if let Some(datadir) = args.datadir {
        profile_from_database(datadir, args.start_block, args.block_count, args.verbose).await?;
    } else {
        println!("Error: --datadir is required");
        println!("\nUsage:");
        println!("  xlayer-trie-profiler --datadir /path/to/reth/datadir --start-block 1000 --block-count 100");
        std::process::exit(1);
    }

    Ok(())
}

async fn profile_from_database(
    datadir: PathBuf,
    start_block: u64,
    block_count: usize,
    verbose: bool,
) -> eyre::Result<()> {
    println!("📂 Opening database at: {}", datadir.display());
    println!("🔍 Analyzing blocks {} to {}\n", start_block, start_block + block_count as u64);

    // This is a simplified example - you'll need to adapt this to your actual DB access patterns
    println!("⚠️  Note: Direct database access implementation needed.");
    println!("    For now, use the in-memory profiler integration (see below)\n");

    // Placeholder for database-based profiling
    // In production, you would:
    // 1. Open ProviderFactory with your database
    // 2. Iterate through blocks
    // 3. Extract TrieUpdates from each ExecutedBlock
    // 4. Calculate statistics

    print_integration_instructions();

    Ok(())
}

fn print_integration_instructions() {
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║         Integration Instructions                              ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    println!("To profile live X Layer blocks, integrate the profiler into your node:");
    println!();
    println!("1. In crates/engine/tree/src/tree/mod.rs (or your block execution handler):");
    println!("   ```rust");
    println!("   use reth_chain_state::TrieProfiler;");
    println!();
    println!("   // Initialize profiler (once at startup)");
    println!("   let profiler = TrieProfiler::new();");
    println!("   profiler.enable();");
    println!();
    println!("   // In block execution loop, after creating ExecutedBlock:");
    println!("   let stats = profiler.profile_trie_updates(");
    println!("       block.number(),");
    println!("       &executed_block.trie_updates,");
    println!("   );");
    println!();
    println!("   // Log every 100 blocks:");
    println!("   if block.number() % 100 == 0 {{");
    println!("       info!(");
    println!("           \"Block #{{}}: {{}} account nodes, {{}} storage nodes, {{}} bytes\",");
    println!("           stats.block_number,");
    println!("           stats.account_nodes_count,");
    println!("           stats.storage_nodes_count,");
    println!("           stats.total_bytes(),");
    println!("       );");
    println!("   }}");
    println!("   ```");
    println!();
    println!("2. Collect stats for 100-1000 blocks and calculate averages");
    println!();
    println!("3. Use AggregatedTrieStats::from_blocks() to generate report");
    println!();
    println!("📊 Metrics will be exposed via Prometheus:");
    println!("   - chain_state_trie_profiler_account_nodes_per_block");
    println!("   - chain_state_trie_profiler_storage_nodes_per_block");
    println!("   - chain_state_trie_profiler_total_nodes_per_block");
    println!("   - chain_state_trie_profiler_total_bytes_per_block");
    println!();
    println!("📈 Query with Grafana or curl:");
    println!("   curl http://localhost:9001/metrics | grep trie_profiler");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_parsing() {
        let args = Args::parse_from(&[
            "xlayer-trie-profiler",
            "--datadir",
            "/tmp/reth",
            "--start-block",
            "5000",
            "--block-count",
            "200",
        ]);

        assert_eq!(args.datadir, Some(PathBuf::from("/tmp/reth")));
        assert_eq!(args.start_block, 5000);
        assert_eq!(args.block_count, 200);
    }
}
