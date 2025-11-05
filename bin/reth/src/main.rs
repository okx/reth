#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use reth::{cli::Cli, ress::install_ress_subprotocol};
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_builder::NodeHandle;
use reth_node_ethereum::EthereumNode;
use tracing::info;

use std::{path::Path, sync::Arc};
use tracing::error;
use xlayer_db::utils::initialize;
use xlayer_exex::utils::post_exec_exex;
use xlayer_rpc::utils::{CustomExt, XlayerExt, XlayerExtApiServer};

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) =
        Cli::<EthereumChainSpecParser, CustomExt>::parse().run(async move |builder, args| {
            let data_dir = builder.config().datadir();
            let db_path = data_dir.db().parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
            let initialize_result = initialize(db_path);

            if let Err(e) = initialize_result {
                error!(target: "reth::cli", "ok Xlayerdb failed to initialize {:#?}", e);
                std::process::exit(1);
            } else {
                info!(target: "reth::cli", "ok Xlayerdb intitialized");
            }

            info!(target: "reth::cli", "Launching node");
            let NodeHandle { node, node_exit_future } = builder
                .node(EthereumNode::default())
                .extend_rpc_modules(move |ctx| {
                    let new_eth_api = ctx.registry.eth_api().clone();
                    let custom_rpc = XlayerExt { backend: Arc::new(new_eth_api) };
                    ctx.modules.merge_configured(custom_rpc.into_rpc())?;
                    info!(target:"reth::cli", "ok Xlayerrpc enabled");

                    Ok(())
                })
                .install_exex("post_exec_exex", |ctx| async move { Ok(post_exec_exex(ctx)) })
                .launch_with_debug_capabilities()
                .await?;

            // Install ress subprotocol.
            if args.ress.enabled {
                install_ress_subprotocol(
                    args.ress,
                    node.provider,
                    node.evm_config,
                    node.network,
                    node.task_executor,
                    node.add_ons_handle.engine_events.new_listener(),
                )?;
            }

            node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
