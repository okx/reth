//! Utility functions for gas price calculations

use alloy_primitives::U256;

/// Calculates the average of two gas prices
pub fn avg_price(low: U256, high: U256) -> U256 {
    (low + high) / U256::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_avg_price() {
        let low = U256::from(100u64);
        let high = U256::from(200u64);
        let avg = avg_price(low, high);
        assert_eq!(avg, U256::from(150u64));
    }
}

