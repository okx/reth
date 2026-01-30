//! Configuration for pre-warming simulation

use std::time::Duration;

/// Configuration for simulation-based pre-warming
#[derive(Debug, Clone)]
pub struct PreWarmingConfig {
    /// Enable pre-warming simulation
    ///
    /// When false, no simulation workers are spawned and no pre-warming occurs.
    /// Default: false (experimental feature, must be explicitly enabled)
    pub enabled: bool,

    /// Number of simulation workers
    ///
    /// Each worker can simulate one transaction at a time.
    /// More workers = more parallel simulation, but also more CPU/memory usage.
    ///
    /// Recommended: 4-8 workers for most systems
    /// X Layer (400ms block time): 4-8 workers recommended
    pub num_workers: usize,

    /// Maximum simulation time before timeout
    ///
    /// If a simulation takes longer than this, it's cancelled.
    /// Prevents hanging on pathological transactions.
    ///
    /// Default: 100ms (should be much less than block time)
    pub simulation_timeout: Duration,

    /// Maximum age of cached keys before eviction
    ///
    /// Keys older than this are considered stale and removed from cache.
    /// Should be longer than typical time between simulation and block building.
    ///
    /// Default: 60 seconds
    pub cache_ttl: Duration,

    /// Maximum number of entries in cache
    ///
    /// Prevents unbounded memory growth.
    /// Each entry is ~500 bytes on average.
    ///
    /// Default: 10,000 entries (~5 MB)
    pub cache_max_entries: usize,

    /// Enable metrics collection
    ///
    /// When true, tracks simulation success/failure rates, cache hit rates, etc.
    /// Default: true
    pub enable_metrics: bool,
}

impl Default for PreWarmingConfig {
    fn default() -> Self {
        Self {
            // Disabled by default - experimental feature
            enabled: false,

            // 4 workers for parallel simulation
            num_workers: 4,

            // 100ms max per simulation (well below typical block times)
            simulation_timeout: Duration::from_millis(100),

            // Keys valid for 60 seconds
            cache_ttl: Duration::from_secs(60),

            // Max 10k cached transaction keys (~5 MB)
            cache_max_entries: 10_000,

            // Metrics enabled by default
            enable_metrics: true,
        }
    }
}

impl PreWarmingConfig {
    /// Create config with pre-warming enabled
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }

    /// Create config with pre-warming disabled
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Set number of workers
    pub fn with_workers(mut self, num_workers: usize) -> Self {
        self.num_workers = num_workers.max(1);  // At least 1 worker
        self
    }

    /// Set simulation timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.simulation_timeout = timeout;
        self
    }

    /// Set cache TTL
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Set cache max entries
    pub fn with_cache_max_entries(mut self, max_entries: usize) -> Self {
        self.cache_max_entries = max_entries;
        self
    }

    /// Disable metrics
    pub fn without_metrics(mut self) -> Self {
        self.enable_metrics = false;
        self
    }

    /// Validate configuration
    ///
    /// Returns error if configuration is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.num_workers == 0 {
            return Err("num_workers must be at least 1".to_string());
        }

        if self.num_workers > 32 {
            return Err("num_workers should not exceed 32 (diminishing returns)".to_string());
        }

        if self.simulation_timeout.as_millis() < 10 {
            return Err("simulation_timeout too low (minimum 10ms)".to_string());
        }

        if self.simulation_timeout.as_secs() > 60 {
            return Err("simulation_timeout too high (maximum 60s)".to_string());
        }

        if self.cache_max_entries == 0 {
            return Err("cache_max_entries must be at least 1".to_string());
        }

        if self.cache_ttl.as_secs() == 0 {
            return Err("cache_ttl must be at least 1 second".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================================
    // Default and Basic Construction Tests
    // ============================================================================

    #[test]
    fn test_default_config() {
        let config = PreWarmingConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.num_workers, 4);
        assert_eq!(config.simulation_timeout, Duration::from_millis(100));
        assert_eq!(config.cache_ttl, Duration::from_secs(60));
        assert_eq!(config.cache_max_entries, 10_000);
        assert!(config.enable_metrics);

        // Default config should be valid
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_enabled_config() {
        let config = PreWarmingConfig::enabled();

        assert!(config.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_disabled_config() {
        let config = PreWarmingConfig::disabled();

        assert!(!config.enabled);
        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Builder Pattern Tests
    // ============================================================================

    #[test]
    fn test_builder_pattern() {
        let config = PreWarmingConfig::enabled()
            .with_workers(8)
            .with_timeout(Duration::from_millis(200))
            .with_cache_ttl(Duration::from_secs(120))
            .with_cache_max_entries(20_000)
            .without_metrics();

        assert!(config.enabled);
        assert_eq!(config.num_workers, 8);
        assert_eq!(config.simulation_timeout, Duration::from_millis(200));
        assert_eq!(config.cache_ttl, Duration::from_secs(120));
        assert_eq!(config.cache_max_entries, 20_000);
        assert!(!config.enable_metrics);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_builder_chaining() {
        let config = PreWarmingConfig::disabled()
            .with_workers(16)
            .with_timeout(Duration::from_millis(500));

        assert!(!config.enabled);
        assert_eq!(config.num_workers, 16);
        assert_eq!(config.simulation_timeout, Duration::from_millis(500));
    }

    #[test]
    fn test_builder_partial_configuration() {
        let config = PreWarmingConfig::enabled()
            .with_workers(2);

        // Other fields should remain default
        assert_eq!(config.simulation_timeout, Duration::from_millis(100));
        assert_eq!(config.cache_ttl, Duration::from_secs(60));
    }

    // ============================================================================
    // Worker Count Validation Tests
    // ============================================================================

    #[test]
    fn test_validation_zero_workers() {
        let config = PreWarmingConfig {
            num_workers: 0,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("num_workers"));
    }

    #[test]
    fn test_validation_one_worker() {
        let config = PreWarmingConfig {
            num_workers: 1,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_too_many_workers() {
        let config = PreWarmingConfig {
            num_workers: 100,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("num_workers"));
    }

    #[test]
    fn test_validation_exactly_32_workers() {
        let config = PreWarmingConfig {
            num_workers: 32,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_33_workers() {
        let config = PreWarmingConfig {
            num_workers: 33,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_min_workers_clamped() {
        let config = PreWarmingConfig::enabled().with_workers(0);

        // Should be clamped to 1
        assert_eq!(config.num_workers, 1);
        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Timeout Validation Tests
    // ============================================================================

    #[test]
    fn test_validation_timeout_too_low() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_millis(5),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("timeout"));
    }

    #[test]
    fn test_validation_timeout_exactly_10ms() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_millis(10),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_timeout_too_high() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_secs(61),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("timeout"));
    }

    #[test]
    fn test_validation_timeout_exactly_60s() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_secs(60),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_timeout_9ms() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_millis(9),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_timeout_zero() {
        let config = PreWarmingConfig {
            simulation_timeout: Duration::from_millis(0),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
    }

    // ============================================================================
    // Cache TTL Validation Tests
    // ============================================================================

    #[test]
    fn test_validation_zero_ttl() {
        let config = PreWarmingConfig {
            cache_ttl: Duration::from_secs(0),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("cache_ttl"));
    }

    #[test]
    fn test_validation_one_second_ttl() {
        let config = PreWarmingConfig {
            cache_ttl: Duration::from_secs(1),
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_very_long_ttl() {
        let config = PreWarmingConfig {
            cache_ttl: Duration::from_secs(3600), // 1 hour
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_ttl_subsecond() {
        let config = PreWarmingConfig {
            cache_ttl: Duration::from_millis(999),
            ..PreWarmingConfig::default()
        };

        // Less than 1 second should fail
        assert!(config.validate().is_err());
    }

    // ============================================================================
    // Cache Max Entries Validation Tests
    // ============================================================================

    #[test]
    fn test_validation_zero_cache_entries() {
        let config = PreWarmingConfig {
            cache_max_entries: 0,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("cache_max_entries"));
    }

    #[test]
    fn test_validation_one_cache_entry() {
        let config = PreWarmingConfig {
            cache_max_entries: 1,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_very_large_cache() {
        let config = PreWarmingConfig {
            cache_max_entries: 1_000_000,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_max_usize_cache() {
        let config = PreWarmingConfig {
            cache_max_entries: usize::MAX,
            ..PreWarmingConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // X-Layer Specific Configuration Tests
    // ============================================================================

    #[test]
    fn test_xlayer_recommended_config() {
        // X-Layer: 400ms block time, need fast simulation
        let config = PreWarmingConfig::enabled()
            .with_workers(8)
            .with_timeout(Duration::from_millis(50))
            .with_cache_ttl(Duration::from_secs(2)); // At least 1 second

        assert!(config.validate().is_ok());
        assert_eq!(config.num_workers, 8);
        assert!(config.simulation_timeout.as_millis() < 100);
    }

    #[test]
    fn test_conservative_config() {
        // Conservative settings for testing
        let config = PreWarmingConfig::enabled()
            .with_workers(2)
            .with_timeout(Duration::from_millis(200))
            .with_cache_ttl(Duration::from_secs(30))
            .with_cache_max_entries(1000);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_aggressive_config() {
        // Aggressive settings for high-throughput
        let config = PreWarmingConfig::enabled()
            .with_workers(32)
            .with_timeout(Duration::from_millis(10))
            .with_cache_ttl(Duration::from_secs(120))
            .with_cache_max_entries(100_000);

        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Metrics Flag Tests
    // ============================================================================

    #[test]
    fn test_metrics_enabled_by_default() {
        let config = PreWarmingConfig::default();
        assert!(config.enable_metrics);
    }

    #[test]
    fn test_metrics_can_be_disabled() {
        let config = PreWarmingConfig::enabled().without_metrics();
        assert!(!config.enable_metrics);
    }

    #[test]
    fn test_metrics_disabled_still_validates() {
        let config = PreWarmingConfig::enabled().without_metrics();
        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Multiple Validation Errors Tests
    // ============================================================================

    #[test]
    fn test_multiple_validation_errors_first_reported() {
        // Multiple errors, but only first is reported
        let config = PreWarmingConfig {
            num_workers: 0,  // Error 1
            simulation_timeout: Duration::from_millis(0),  // Error 2
            ..PreWarmingConfig::default()
        };

        let result = config.validate();
        assert!(result.is_err());
        // Should report first error (num_workers)
        assert!(result.unwrap_err().contains("num_workers"));
    }

    // ============================================================================
    // Edge Case: Boundary Value Tests
    // ============================================================================

    #[test]
    fn test_boundary_1_worker() {
        assert!(PreWarmingConfig::enabled().with_workers(1).validate().is_ok());
    }

    #[test]
    fn test_boundary_32_workers() {
        assert!(PreWarmingConfig::enabled().with_workers(32).validate().is_ok());
    }

    #[test]
    fn test_boundary_10ms_timeout() {
        let config = PreWarmingConfig::enabled()
            .with_timeout(Duration::from_millis(10));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_boundary_60s_timeout() {
        let config = PreWarmingConfig::enabled()
            .with_timeout(Duration::from_secs(60));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_boundary_1s_ttl() {
        let config = PreWarmingConfig::enabled()
            .with_cache_ttl(Duration::from_secs(1));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_boundary_1_cache_entry() {
        let config = PreWarmingConfig::enabled()
            .with_cache_max_entries(1);
        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Clone and Copy Tests
    // ============================================================================

    #[test]
    fn test_config_clone() {
        let config1 = PreWarmingConfig::enabled()
            .with_workers(8);

        let config2 = config1.clone();

        assert_eq!(config1.enabled, config2.enabled);
        assert_eq!(config1.num_workers, config2.num_workers);
    }

    #[test]
    fn test_config_clone_independence() {
        let config1 = PreWarmingConfig::enabled();
        let mut config2 = config1.clone();

        config2.num_workers = 16;

        // Original shouldn't change
        assert_eq!(config1.num_workers, 4);
        assert_eq!(config2.num_workers, 16);
    }

    // ============================================================================
    // Realistic Production Configurations
    // ============================================================================

    #[test]
    fn test_production_config_mainnet() {
        // Mainnet-like: 12s block time, conservative
        let config = PreWarmingConfig::enabled()
            .with_workers(4)
            .with_timeout(Duration::from_millis(500))
            .with_cache_ttl(Duration::from_secs(30))
            .with_cache_max_entries(50_000);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_production_config_l2_fast() {
        // L2 fast: 2s block time
        let config = PreWarmingConfig::enabled()
            .with_workers(8)
            .with_timeout(Duration::from_millis(100))
            .with_cache_ttl(Duration::from_secs(10))
            .with_cache_max_entries(20_000);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_production_config_testnet() {
        // Testnet: Can be more aggressive
        let config = PreWarmingConfig::enabled()
            .with_workers(16)
            .with_timeout(Duration::from_millis(50))
            .with_cache_ttl(Duration::from_secs(60))
            .with_cache_max_entries(10_000);

        assert!(config.validate().is_ok());
    }

    // ============================================================================
    // Stress Test: Extreme Values
    // ============================================================================

    #[test]
    fn test_extreme_worker_count_max() {
        let config = PreWarmingConfig::enabled().with_workers(usize::MAX);
        // Should be clamped or validated
        // Since validation checks > 32, this should fail
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_extreme_timeout_max_duration() {
        let config = PreWarmingConfig::enabled()
            .with_timeout(Duration::MAX);

        // Should fail validation (> 60s)
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_extreme_cache_entries_max() {
        let config = PreWarmingConfig::enabled()
            .with_cache_max_entries(usize::MAX);

        // Should pass validation (no upper limit on cache entries)
        assert!(config.validate().is_ok());
    }
}

