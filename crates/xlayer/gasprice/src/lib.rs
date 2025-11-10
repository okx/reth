//! XLayer gas price oracle implementation
//!
//! This crate provides gas price calculation strategies for XLayer:
//! - Default: Uses a fixed default gas price from configuration
//! - Follower: Calculates gas price based on L1 gas price and coin prices
//! - Fixed: Uses a fixed USDT price converted to native token

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

/// Gas price cache implementation
pub mod cache;

/// Default gas price strategy
pub mod default;

/// Gas price scheduler
pub mod scheduler;

/// Gas price suggester interface
pub mod suggester;

/// Utility functions
pub mod utils;

// Re-exports
pub use cache::GasPriceCache;
pub use scheduler::XLayerScheduler;
pub use suggester::NewL2GasPriceSuggester;
// Re-export L2GasPricer from rpc-eth-api for backward compatibility
pub use reth_rpc_eth_api::helpers::pricer::L2GasPricer;
// Re-export GasPriceCacheTrait from rpc-eth-api for backward compatibility
pub use reth_rpc_eth_api::helpers::pricer::GasPriceCacheTrait;

/// Default XLayer gas price (1 GWei)
pub const DEFAULT_XLAYER_PRICE: u64 = 1_000_000_000; // 1 GWei

/// Maximum cache size for raw gas prices
pub const MAX_CACHE_SIZE: usize = 30;

/// Minimum gas price window size for recent calculations
pub const MIN_GP_WINDOW_SIZE: usize = 27;

