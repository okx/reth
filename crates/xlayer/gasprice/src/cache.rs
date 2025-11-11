//! Gas price cache implementation for XLayer
//!
//! Provides caching mechanisms for raw gas prices with a circular buffer
//! and atomic operations for thread-safe access.

use alloy_primitives::U256;
use parking_lot::RwLock;
use reth_rpc_eth_api::helpers::pricer::GasPriceCacheTrait;

/// Simple gas price cache implementation for default mode
/// In default mode, latest price = latest raw gas price
#[derive(Debug)]
pub struct GasPriceCache {
    latest_price: RwLock<U256>,
}

impl GasPriceCache {
    /// Creates a new gas price cache
    pub fn new() -> Self {
        Self {
            latest_price: RwLock::new(U256::ZERO),
        }
    }
}

impl Default for GasPriceCache {
    fn default() -> Self {
        Self::new()
    }
}

// TODO: implement the cache for follower and fixed modes
// This is cache is only be used in default mode
impl GasPriceCacheTrait for GasPriceCache {
    fn get_latest(&self) -> U256 {
        *self.latest_price.read()
    }

    fn set_latest(&self, price: U256) {
        *self.latest_price.write() = price;
    }

    fn get_latest_raw_gp(&self) -> U256 {
        *self.latest_price.read()
    }

    fn set_latest_raw_gp(&self, rgp: U256) {
        *self.latest_price.write() = rgp;
    }

    fn get_min_raw_gp_recent(&self) -> U256 {
        *self.latest_price.read()
    }
}
