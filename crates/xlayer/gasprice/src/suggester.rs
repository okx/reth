//! Gas price suggester interface and factory
//!
//! Provides the main interface for gas price calculation strategies
//! and a factory function to create the appropriate suggester based on configuration.

use reth_rpc_eth_api::helpers::pricer::L2GasPricer;
use std::sync::Arc;

use crate::{
    default::DefaultGasPricer,
    // fixed::FixedGasPricer,
    // follower::FollowerGasPricer,
};

/// Trait for XLayer gas price arguments
pub trait XLayerGasPriceArgsTrait {
    fn price_type(&self) -> Option<&str>;
    fn default(&self) -> Option<alloy_primitives::U256>;
}

/// Creates a new L2 gas price suggester based on the configuration
///
/// # Arguments
///
/// * `args` - The XLayer gas price arguments
///
/// # Returns
///
/// Returns an Arc-wrapped implementation of `L2GasPricer` based on the price type:
/// - `Default`: Uses a fixed default gas price
/// - `Follower`: Calculates based on L1 gas price and coin prices
/// - `Fixed`: Uses a fixed USDT price converted to native token
pub fn new_l2_gas_price_suggester<T: XLayerGasPriceArgsTrait>(args: &T) -> Arc<dyn L2GasPricer> {
    let price_type = args.price_type().unwrap_or("");
    match price_type {
        "default" => {
            tracing::info!("Creating Default gas price suggester");
            Arc::new(DefaultGasPricer::new(args.default()))
        }
        // "follower" => {
        //     tracing::info!("Creating Follower gas price suggester");
        //     Arc::new(FollowerGasPricer::new(args))
        // }
        // "fixed" => {
        //     tracing::info!("Creating Fixed gas price suggester");
        //     Arc::new(FixedGasPricer::new(args))
        // }
        _ => {
            tracing::error!("Invalid gas price type: {}", price_type);
            panic!("Invalid gas price type: {}", price_type);
        }
    }
}

/// Type alias for the suggester factory function
pub type NewL2GasPriceSuggester<T> = fn(&T) -> Arc<dyn L2GasPricer>;

