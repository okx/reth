//! Apollo configuration client for dynamic configuration management.

pub mod client;
pub mod handler;
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
pub use handler::ApolloHandler;
pub use types::ApolloConfig;
