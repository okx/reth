use crate::handler::ApolloHandler;
use crate::types::{ApolloConfig, ApolloError};
use apollo_sdk::client::apollo_config_client::ApolloConfigClient;
use async_once_cell::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

/// Apollo client wrapper for reth
#[derive(Clone)]
pub struct ApolloClient {
    pub inner: Arc<RwLock<ApolloConfigClient>>,
    pub config: ApolloConfig,
    pub namespace_map: HashMap<String, String>,
    pub flags: HashMap<String, JsonValue>,
    // For forward compatibility if multiple revm clients are present in the future, we can reuse this crate
    // and add the handler to the client.
    pub handler: Option<Arc<dyn ApolloHandler>>,
}

/// Singleton instance
static INSTANCE: OnceCell<ApolloClient> = OnceCell::new();

impl ApolloClient {
    /// Get singleton instance
    pub async fn get_instance(
        config: ApolloConfig,
        flags: HashMap<String, JsonValue>,
    ) -> Result<ApolloClient, ApolloError> {
        INSTANCE
            .get_or_try_init(Self::new_instance(config, flags))
            .await
            .map(|client| client.clone())
    }

    /// Create new instance
    async fn new_instance(
        config: ApolloConfig,
        flags: HashMap<String, JsonValue>,
    ) -> Result<ApolloClient, ApolloError> {
        // Validate configuration
        if config.app_id.is_empty()
            || config.meta_server.is_empty()
            || config.cluster_name.is_empty()
        {
            return Err(ApolloError::ClientInit(
                "apollo enabled but config is not valid".to_string(),
            ));
        }

        let client: ApolloConfigClient = apollo_sdk::client::apollo_config_client::new(
            config.meta_server.iter().map(|s| s.as_str()).collect(),
            &config.app_id,
            &config.cluster_name,
            config.namespaces.as_deref().map(|ns| ns.iter().map(|s| s.as_str()).collect()),
            config.secret.as_deref(),
        )
        .await
        .map_err(|e| ApolloError::ClientInit(e.to_string()))?;

        // Create namespace map
        let mut namespace_map = HashMap::new();
        if let Some(namespaces) = &config.namespaces {
            for namespace in namespaces {
                let prefix = get_namespace_prefix(namespace)?;
                if namespace_map.contains_key(&prefix) {
                    return Err(ApolloError::ClientInit(format!(
                        "duplicate apollo namespace: {}",
                        prefix
                    )));
                }
                namespace_map.insert(prefix, namespace.clone());
            }
        }

        Ok(ApolloClient {
            inner: Arc::new(RwLock::new(client)),
            config,
            namespace_map,
            flags,
            handler: None,
        })
    }

    /// Add handler for config changes
    pub fn add_handler(&mut self, handler: Arc<dyn ApolloHandler>) {
        self.handler = Some(handler);
    }

    /// Parse YAML config and create context with flags
    /// Equivalent to GetConfigContext in op-geth
    ///
    /// Returns a tuple of:
    /// - HashMap: Updated flags context
    pub fn get_config_context(
        &self,
        yaml_value: &str,
    ) -> Result<HashMap<String, JsonValue>, ApolloError> {
        // Parse YAML into a map
        let config: HashMap<String, JsonValue> = serde_yaml::from_str(yaml_value)
            .map_err(|e| ApolloError::ParseError(format!("Failed to parse YAML: {}", e)))?;

        // Create a copy of current flags as base context
        let mut ctx = self.flags.clone();

        // Iterate over config and set flags that aren't already set
        for (key, value) in &config {
            if !self.is_flag_set(key) {
                self.set_flag_value(&mut ctx, key, value)?;
            }
        }

        Ok(ctx)
    }

    /// Check if a flag is already set in the current flags
    fn is_flag_set(&self, key: &str) -> bool {
        self.flags.contains_key(key)
    }

    /// Set flag value in the context, handling arrays by joining with commas
    fn set_flag_value(
        &self,
        ctx: &mut HashMap<String, JsonValue>,
        key: &str,
        value: &JsonValue,
    ) -> Result<(), ApolloError> {
        let string_value = match value {
            JsonValue::Array(arr) => {
                // Convert array to comma-separated string
                arr.iter()
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            }
            _ => value.to_string().trim_matches('"').to_string(),
        };

        // Store in the flags HashMap
        ctx.insert(key.to_string(), JsonValue::String(string_value));
        Ok(())
    }

    /// Load config from Apollo
    pub async fn load_config(&self) -> Result<bool, ApolloError> {
        let client = self.inner.read().await;
        for (prefix, namespace) in &self.namespace_map {
            let config = client.get_config(namespace);
            // Get config cache for namespace
            let ctx = self.get_config_context(&config.unwrap().config_value);
            if let Some(handler) = &self.handler {
                handler.load_config(prefix, ctx?);
            }
        }

        Ok(true)
    }

    /// Check if instance is initialized
    pub fn is_initialized() -> bool {
        INSTANCE.get().is_some()
    }
}

/// Get namespace prefix from full namespace name
fn get_namespace_prefix(namespace: &str) -> Result<String, ApolloError> {
    namespace
        .split('-')
        .next()
        .ok_or_else(|| ApolloError::InvalidNamespace(namespace.to_string()))
        .map(|s| s.to_string())
}
