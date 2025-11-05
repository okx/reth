//! L2 gas price pricer trait definition
//!
//! This module defines the `L2GasPricer` trait that is used for XLayer gas price calculations.
//! The trait is defined here to avoid circular dependencies between `rpc-eth-api` and
//! `xlayer-gasprice` crates.

use alloy_primitives::U256;
use std::sync::Arc;
use std::any::Any;

/// Gas price cache trait for accessing cached gas price data
pub trait GasPriceCacheTrait: Send + Sync {
    /// Gets the latest cached gas price
    fn get_latest(&self) -> U256;
    
    /// Sets the latest cached gas price
    fn set_latest(&self, price: U256);
    
    /// Gets the latest raw gas price
    fn get_latest_raw_gp(&self) -> U256;
    
    /// Sets the latest raw gas price
    fn set_latest_raw_gp(&self, price: U256);
    
    /// Gets the minimum raw gas price from recent history
    fn get_min_raw_gp_recent(&self) -> U256;
}

/// Interface for L2 gas price calculation strategies
pub trait L2GasPricer: Send + Sync {
    /// Updates the gas price average based on L1 gas price
    fn update_gas_price_avg(&self, l1_gas_price: U256);

    /// Updates the configuration
    /// The args parameter should be a reference to XLayerGasPriceArgs, but we use &dyn Any
    /// to avoid circular dependencies. Implementations should downcast to the appropriate type.
    fn update_config(&self, args: &dyn Any);

    /// Gets the last calculated raw gas price
    fn get_last_raw_gp(&self) -> U256;

    /// Gets the gas price cache
    fn get_gas_cache(&self) -> Arc<dyn GasPriceCacheTrait>;
}

