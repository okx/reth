//! Apollo handler interface for dynamic configuration loading

use serde_json::Value;
use std::collections::HashMap;

/// Handler interface for Apollo config changes
pub trait ApolloHandler: Send + Sync {
    /// Handle configuration changes from Apollo
    fn handle_config_change(&self, prefix: &str, key: &str, value: &Value);

    /// Load initial configuration from Apollo
    fn load_config(&self, prefix: &str, flags: HashMap<String, Value>);

    /// Get the current configuration given a key
    fn get_config(&self, key: &str) -> Value;
}
