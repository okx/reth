//! Legacy RPC initialization helpers
//!
//! This module provides clean initialization logic for legacy RPC components,
//! reducing code intrusion in core RPC modules.

use crate::{CrossBoundaryFilterManager, LegacyRpcClient, LegacyRpcConfig};
use std::sync::Arc;

/// Initialized legacy RPC components
#[derive(Debug)]
pub struct LegacyRpcComponents {
    /// The legacy RPC HTTP client
    pub client: Option<Arc<LegacyRpcClient>>,
    /// The cross-boundary filter manager
    pub filter_manager: Option<Arc<CrossBoundaryFilterManager>>,
}

impl LegacyRpcComponents {
    /// Create empty components (no legacy RPC configured)
    pub fn empty() -> Self {
        Self { client: None, filter_manager: None }
    }

    /// Check if legacy RPC is enabled
    pub fn is_enabled(&self) -> bool {
        self.client.is_some()
    }
}

/// Initialize legacy RPC components from config
///
/// This function encapsulates all the initialization logic, error handling,
/// and logging for legacy RPC support.
///
/// # Arguments
/// * `config` - Optional legacy RPC configuration
///
/// # Returns
/// Initialized components. If initialization fails, returns empty components
/// and logs a warning.
///
/// # Example
/// ```ignore
/// let legacy = init_legacy_rpc_components(legacy_config);
/// let eth_api = EthApiInner::new(
///     provider,
///     legacy.client,
///     legacy.filter_manager,
///     // ... other params
/// );
/// ```
pub fn init_legacy_rpc_components(config: Option<LegacyRpcConfig>) -> LegacyRpcComponents {
    let Some(config) = config else {
        return LegacyRpcComponents::empty();
    };

    match LegacyRpcClient::from_config(&config) {
        Ok(client) => {
            tracing::info!(
                cutoff_block = config.cutoff_block,
                endpoint = %config.endpoint,
                "Legacy RPC support initialized"
            );

            let filter_manager = CrossBoundaryFilterManager::new(config.cutoff_block);

            LegacyRpcComponents {
                client: Some(Arc::new(client)),
                filter_manager: Some(Arc::new(filter_manager)),
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                endpoint = %config.endpoint,
                "Failed to initialize legacy RPC client, legacy support disabled"
            );
            LegacyRpcComponents::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_empty_components() {
        let components = LegacyRpcComponents::empty();
        assert!(components.client.is_none());
        assert!(components.filter_manager.is_none());
        assert!(!components.is_enabled());
    }

    #[test]
    fn test_init_without_config() {
        let components = init_legacy_rpc_components(None);
        assert!(!components.is_enabled());
    }

    #[test]
    fn test_init_with_invalid_url() {
        let config = LegacyRpcConfig::new(
            1000000,
            "invalid://url".to_string(),
            Duration::from_secs(30),
        );
        let components = init_legacy_rpc_components(Some(config));
        assert!(!components.is_enabled());
    }
}

