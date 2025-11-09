#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::{args::RollupArgs, OpNode};
use tracing::info;

use std::{path::Path, sync::Arc};
use tracing::error;
use xlayer_db::utils::initialize;
use xlayer_exex::utils::post_exec_exex;
use xlayer_rpc::utils::{CustomExt, XlayerExt, XlayerExtApiServer};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    if let Err(err) =
        Cli::<OpChainSpecParser, CustomExt>::parse().run(async move |builder, args| {
            let data_dir = builder.config().datadir();
            let db_path = data_dir.db().parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
            let initialize_result = initialize(db_path);

            if let Err(e) = initialize_result {
                error!(target: "reth::cli", "xlayer db failed to initialize {:#?}", e);
            } else {
                info!(target: "reth::cli", "xlayer db initialized");
            }

            info!(target: "reth::cli", "Launching node");
            let handle = builder
                .node(OpNode::new(args.rollup_args.clone()))
                .extend_rpc_modules(move |ctx| {
                    let new_op_eth_api = ctx.registry.eth_api().clone();
                    let custom_rpc = XlayerExt { backend: Arc::new(new_op_eth_api) };
                    ctx.modules.merge_configured(custom_rpc.into_rpc())?;
                    info!(target:"reth::cli", "xlayer op rpc enabled");

                    Ok(())
                })
                .install_exex("post_exec_exex", |ctx| async move { Ok(post_exec_exex(ctx)) })
                .launch_with_debug_capabilities()
                .await?;
            handle.node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
