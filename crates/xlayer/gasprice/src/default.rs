//! Default gas price strategy
//!
//! Uses a fixed default gas price from the configuration.
//! This is the simplest strategy and doesn't require external data sources.

use alloy_primitives::U256;
use parking_lot::RwLock;
use reth_rpc_eth_api::helpers::pricer::{GasPriceCacheTrait, L2GasPricer};
use std::sync::Arc;

use crate::{cache::GasPriceCache, DEFAULT_XLAYER_PRICE};

/// Default gas price suggester
///
/// Always returns the configured default gas price without any calculations.
#[derive(Debug)]
pub struct DefaultGasPricer {
    /// Default gas price value
    default_price: U256,
    /// Last calculated raw gas price
    last_raw_gp: RwLock<U256>,
    /// Gas price cache
    gas_cache: Arc<GasPriceCache>,
}

impl DefaultGasPricer {
    /// Creates a new default gas price suggester
    pub fn new(default_price: Option<U256>) -> Self {
        let default_price = default_price.unwrap_or(U256::from(DEFAULT_XLAYER_PRICE));
        Self {
            default_price,
            last_raw_gp: RwLock::new(default_price),
            gas_cache: Arc::new(GasPriceCache::new()),
        }
    }
}

impl L2GasPricer for DefaultGasPricer {
    fn update_gas_price_avg(&self, _l1_gas_price: U256) {
        // For default strategy, always use the configured default price
        *self.last_raw_gp.write() = self.default_price;
        tracing::debug!(
            price = %self.default_price,
            "Default gas price strategy: using configured default"
        );
    }

    fn update_config(&self, _args: &dyn std::any::Any) {
        // For default strategy, config updates are not needed
        tracing::debug!("Default gas price strategy: config update ignored");
    }

    fn get_last_raw_gp(&self) -> U256 {
        *self.last_raw_gp.read()
    }

    fn get_gas_cache(&self) -> Arc<dyn GasPriceCacheTrait> {
        Arc::clone(&self.gas_cache) as Arc<dyn GasPriceCacheTrait>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_rpc_eth_api::helpers::pricer::GasPriceCacheTrait;

    #[test]
    fn test_default_pricer_returns_configured_price() {
        let default_price = Some(U256::from(1_000_000_000u64)); // 1 GWei
        let pricer = DefaultGasPricer::new(default_price);

        // Update with any L1 price, should still return default
        pricer.update_gas_price_avg(U256::from(50_000_000_000u64));

        assert_eq!(pricer.get_last_raw_gp(), U256::from(1_000_000_000u64));
    }

    #[test]
    fn test_default_gas_price_cache_operations() {
        let cache = GasPriceCache::new();
        cache.set_latest(U256::from(200));
        assert_eq!(cache.get_latest(), U256::from(200));
        assert_eq!(cache.get_latest_raw_gp(), U256::from(200));
        assert_eq!(cache.get_min_raw_gp_recent(), U256::from(200));

        cache.set_latest_raw_gp(U256::from(300));
        assert_eq!(cache.get_latest(), U256::from(300));
    }
}

