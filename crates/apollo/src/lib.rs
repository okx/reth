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
        let ns = $namespace;  // Bind to extend lifetime
        let ns_ref: &str = &ns;

        let result =$crate::client::ApolloClient::get_instance()
            .ok()
            .and_then(|apollo| apollo.get_cached_config(ns_ref, $key))
            .and_then(|v| $crate::types::FromJsonValue::from_json_value(&v));

        match result {
            Some(value) => value,
            None => {
                tracing::debug!(
                    target: "reth::apollo",
                    namespace = ns_ref,
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
