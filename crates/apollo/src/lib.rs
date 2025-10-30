//! Apollo configuration client for dynamic configuration management.

/// Apollo client module
pub mod client;
/// Apollo namespace module
pub mod namespace;
/// Apollo types and configuration
pub mod types;

/// This is documentation for the macro
#[macro_export]
macro_rules! apollo_cached_config {
    ($namespace:expr, $key:expr, $default:expr) => {{
        async {
            let apollo = $crate::client::ApolloClient::get_instance().ok();
            if let Some(apollo) = apollo {
                apollo
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
pub use types::ApolloConfig;
