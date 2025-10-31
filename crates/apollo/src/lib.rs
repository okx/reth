//! Apollo configuration client for dynamic configuration management.

/// Apollo client module
pub mod client;
/// Apollo namespace module
pub mod namespace;
/// Apollo types and configuration
pub mod types;

/// This is documentation for the macro
#[macro_export]
macro_rules! apollo_config_or {
    ($namespace:expr, $key:expr, $default:expr) => {{
        let result =$crate::client::ApolloClient::get_instance()
            .ok()
            .and_then(|apollo| apollo.get_cached_config($namespace, $key))
            .and_then(|v| $crate::types::FromJsonValue::from_json_value(&v));

        match result {
            Some(value) => value,
            None => {
                tracing::debug!(
                    target: "reth::apollo",
                    namespace = $namespace,
                    key = $key,
                    default = ?$default,
                    "Using default config (client not initialized, key missing, or type mismatch)"
                );
                $default
            }
        }
    }};
}

pub use client::ApolloClient;
pub use types::ApolloConfig;
