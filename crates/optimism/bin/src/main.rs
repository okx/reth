#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use reth_apollo::{ApolloConfig, ApolloService};
use reth_node_builder::Node;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::{args::RollupArgs, OpNode};
use tracing::info;

use op_rbuilder::{
    args::OpRbuilderArgs,
    builders::{BuilderConfig, FlashblocksServiceBuilder},
};
use std::{path::Path, sync::Arc};
use tracing::error;
use xlayer_db::utils::initialize;
use xlayer_exex::utils::post_exec_exex;
use xlayer_rpc::utils::{XlayerExt, XlayerExtApiServer};

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
        Cli::<OpChainSpecParser, OpRbuilderArgs>::parse().run(async move |builder, parsed_args| {
            info!(target: "reth::cli", "Launching node");
            let rollup_args = parsed_args.rollup_args.clone();

            // For X Layer
            if rollup_args.xlayer_args.apollo.enabled {
                run_apollo(&rollup_args).await;
            }

            let enable_inner_tx = rollup_args.xlayer_args.enable_inner_tx;
            let data_dir = builder.config().datadir();
            let op_node = OpNode::new(rollup_args);
            if parsed_args.flashblocks.enabled {
                let builder_config = BuilderConfig::try_from(parsed_args.clone())
                    .expect("Failed to convert builder args to builder config");
                let components =
                    op_node.components().payload(FlashblocksServiceBuilder(builder_config));
                let mut node_builder = builder
                    .with_types::<OpNode>()
                    .with_components(components)
                    .with_add_ons(op_node.add_ons());

                if enable_inner_tx {
                    // Conditionally initialize InnerTx database before consuming builder
                    let db_path =
                        data_dir.db().parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
                    match initialize(db_path) {
                        Ok(_) => info!(target: "reth::cli", "xlayer db initialized"),
                        Err(e) => {
                            error!(target: "reth::cli", "xlayer db failed to initialize {:#?}", e)
                        }
                    }

                    node_builder = node_builder
                        .extend_rpc_modules(move |ctx| {
                            let new_op_eth_api = ctx.registry.eth_api().clone();
                            let custom_rpc = XlayerExt { backend: Arc::new(new_op_eth_api) };
                            ctx.modules.merge_configured(custom_rpc.into_rpc())?;
                            info!(target:"reth::cli", "xlayer innertx rpc enabled");
                            Ok(())
                        })
                        .install_exex(
                            "post_exec_exex",
                            |ctx| async move { Ok(post_exec_exex(ctx)) },
                        );
                }

                let handle = node_builder.launch_with_debug_capabilities().await?;
                return handle.node_exit_future.await;
            }

            let mut node_builder = builder.node(op_node);

            if enable_inner_tx {
                // Conditionally initialize InnerTx database before consuming builder
                let db_path =
                    data_dir.db().parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
                match initialize(db_path) {
                    Ok(_) => info!(target: "reth::cli", "xlayer db initialized"),
                    Err(e) => {
                        error!(target: "reth::cli", "xlayer db failed to initialize {:#?}", e)
                    }
                }

                node_builder = node_builder
                    .extend_rpc_modules(move |ctx| {
                        let new_op_eth_api = ctx.registry.eth_api().clone();
                        let custom_rpc = XlayerExt { backend: Arc::new(new_op_eth_api) };
                        ctx.modules.merge_configured(custom_rpc.into_rpc())?;
                        info!(target:"reth::cli", "xlayer innertx rpc enabled");
                        Ok(())
                    })
                    .install_exex("post_exec_exex", |ctx| async move { Ok(post_exec_exex(ctx)) });
            }

            let handle = node_builder.launch_with_debug_capabilities().await?;

            handle.node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

async fn run_apollo(rollup_args: &RollupArgs) {
    tracing::info!(target: "reth::apollo", "[Apollo] Apollo enabled: {:?}", rollup_args.xlayer_args.apollo.enabled);
    tracing::info!(target: "reth::apollo", "[Apollo] Apollo app ID: {:?}", rollup_args.xlayer_args.apollo.apollo_app_id);
    tracing::info!(target: "reth::apollo", "[Apollo] Apollo IP: {:?}", rollup_args.xlayer_args.apollo.apollo_ip);
    tracing::info!(target: "reth::apollo", "[Apollo] Apollo cluster: {:?}", rollup_args.xlayer_args.apollo.apollo_cluster);
    tracing::info!(target: "reth::apollo", "[Apollo] Apollo namespace: {:?}", rollup_args.xlayer_args.apollo.apollo_namespace);

    // Create Apollo config from args
    let apollo_config = ApolloConfig {
        meta_server: vec![rollup_args.xlayer_args.apollo.apollo_ip.to_string()],
        app_id: rollup_args.xlayer_args.apollo.apollo_app_id.to_string(),
        cluster_name: rollup_args.xlayer_args.apollo.apollo_cluster.to_string(),
        namespaces: Some(
            rollup_args
                .xlayer_args
                .apollo
                .apollo_namespace
                .split(',')
                .map(|s| s.to_string())
                .collect(),
        ),
        secret: None,
    };

    tracing::info!(target: "reth::apollo", "[Apollo] Creating Apollo config");

    // Initialize Apollo singleton
    if let Err(e) = ApolloService::try_initialize(apollo_config).await {
        tracing::error!(target: "reth::apollo", "[Apollo] Failed to initialize Apollo: {:?}; Proceeding with node launch without Apollo", e);
    } else {
        tracing::info!(target: "reth::apollo", "[Apollo] Apollo initialized successfully")
    }
}
