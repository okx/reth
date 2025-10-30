//! Apollo configuration client for dynamic configuration management.

/// Apollo client module
pub mod client;
/// Apollo handler module
pub mod handler;
/// Apollo namespace module
pub mod namespace;
/// Apollo types and configuration
pub mod types;

/// This is documentation for the macro
#[macro_export]
macro_rules! apollo_cached_config {
    ($namespace:expr, $key:expr, $default:expr) => {{
        async {
            tracing::info!(target: "reth::apollo", "Macro: Apollo config for namespace: {:?}, key: {:?}", $namespace, $key);
            let apollo = $crate::client::ApolloClient::get_instance().ok();
            if let Some(apollo) = apollo {
                tracing::info!(target: "reth::apollo", "Macro: Apollo client found");
                let client = apollo.read().await;
                client
                    .get_cached_config($namespace, $key)
                    .await
                    .and_then(|v| $crate::types::FromJsonValue::from_json_value(&v))
                    .unwrap_or($default)
            } else {
                $default
            }
        }
    }};
}

pub use client::ApolloClient;
pub use handler::ApolloHandler;
pub use types::ApolloConfig;
