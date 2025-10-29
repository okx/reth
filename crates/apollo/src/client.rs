use crate::types::{ApolloConfig, ApolloError};
use apollo_sdk::client::apollo_config_client::ApolloConfigClient;
use async_once_cell::OnceCell;
use serde_json::Value as JsonValue;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Apollo client wrapper for reth
pub struct ApolloClient {
    pub inner: Arc<RwLock<ApolloConfigClient>>,
    pub config: ApolloConfig,
    pub namespace_map: HashMap<String, String>,
    pub flags: HashMap<String, JsonValue>,

    pub cache: Arc<RwLock<HashMap<String, JsonValue>>>,
    pub listener_state: Arc<Mutex<ListenerState>>,
}

pub struct ListenerState {
    task: Option<tokio::task::JoinHandle<()>>,
    shutdown_tx: Option<tokio::sync::mpsc::Sender<()>>,
}

/// Singleton instance
static INSTANCE: OnceCell<ApolloClient> = OnceCell::new();
const POLL_INTERVAL_SECS: u64 = 1;

impl std::fmt::Debug for ApolloClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApolloClient")
            .field("config", &self.config)
            .field("namespace_map", &self.namespace_map)
            .field("flags", &self.flags)
            .field("cache", &self.cache)
            .field("listener_state", &"<locked>")
            .finish()
    }
}

impl Clone for ApolloClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            config: self.config.clone(),
            namespace_map: self.namespace_map.clone(),
            flags: self.flags.clone(),
            cache: self.cache.clone(),
            listener_state: self.listener_state.clone(),
        }
    }
}

impl ApolloClient {
    /// Get singleton instance
    pub async fn get_instance(
        config: ApolloConfig,
        flags: HashMap<String, JsonValue>,
    ) -> Result<ApolloClient, ApolloError> {
        info!(target: "reth::apollo", "[Apollo] Getting Apollo client");
        let client = INSTANCE
            .get_or_try_init(Self::new_instance(config, flags))
            .await
            .map(|client| client.clone())?;

        // Start listening on the singleton instance
        if let Some(singleton) = INSTANCE.get() {
            singleton.start_listening().await?;
        }

        Ok(client)
    }

    /// Create new instance
    async fn new_instance(
        config: ApolloConfig,
        flags: HashMap<String, JsonValue>,
    ) -> Result<ApolloClient, ApolloError> {
        info!(target: "reth::apollo", "[Apollo] Creating new instance");
        // Validate configuration
        if config.app_id.is_empty()
            || config.meta_server.is_empty()
            || config.cluster_name.is_empty()
        {
            return Err(ApolloError::ClientInit(
                "apollo enabled but config is not valid".to_string(),
            ));
        }

        info!(target: "reth::apollo", "[Apollo] Namespaces: {:?}", config.namespaces);

        let client: ApolloConfigClient = apollo_sdk::client::apollo_config_client::new(
            config.meta_server.iter().map(|s| s.as_str()).collect(),
            &config.app_id,
            &config.cluster_name,
            config.namespaces.as_deref().map(|ns| ns.iter().map(|s| s.as_str()).collect()),
            config.secret.as_deref(),
        )
        .await
        .map_err(|e| {
            error!(target: "reth::apollo", "[Apollo] Failed to create client: {:?}", e);
            ApolloError::ClientInit(format!("Failed to connect to Apollo: {}. Check if Apollo service is accessible and configuration is correct.", e))
        })?;

        // Create namespace map
        let mut namespace_map = HashMap::new();
        if let Some(namespaces) = &config.namespaces {
            for namespace in namespaces {
                let prefix = get_namespace_prefix(namespace)?;
                info!(target: "reth::apollo", "[Apollo] Namespace prefix: {:?}", prefix);
                if namespace_map.contains_key(&prefix) {
                    return Err(ApolloError::ClientInit(format!(
                        "duplicate apollo namespace: {}",
                        prefix
                    )));
                }
                namespace_map.insert(prefix, namespace.clone());
                info!(target: "reth::apollo", "[Apollo] Namespace map: {:?}", namespace_map);
            }
        }

        info!(target: "reth::apollo", "[Apollo] New instance created");

        Ok(ApolloClient {
            inner: Arc::new(RwLock::new(client)),
            config,
            namespace_map,
            flags,
            cache: Arc::new(RwLock::new(HashMap::new())),
            listener_state: Arc::new(Mutex::new(ListenerState { task: None, shutdown_tx: None })),
        })
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
        for (_, namespace) in &self.namespace_map {
            let config = client.get_config_from_namespace("content", namespace);
            if let Some(config) = config {
                // Get config cache for namespace
                Self::update_cache_from_config(self.cache.clone(), namespace, &config.config_value)
                    .await;
            } else {
                warn!(target: "reth::apollo", "[Apollo] No config found for namespace {}", namespace);
            }
        }

        Ok(true)
    }

    /// Check if instance is initialized
    pub fn is_initialized() -> bool {
        INSTANCE.get().is_some()
    }

    // Start continuous listening
    pub async fn start_listening(&self) -> Result<(), ApolloError> {
        let mut state = self.listener_state.lock().await;
        if state.task.is_some() {
            return Ok(()); // Already listening
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        let client = self.inner.clone();
        let cache = self.cache.clone();
        let namespace_map = self.namespace_map.clone();

        // Start listening to all namespaces
        let client_read = client.read().await;
        for (_prefix, namespace) in &namespace_map {
            let listen_res = client_read.listen_namespace(namespace).await;
            if listen_res.is_some() {
                error!(target: "reth::apollo", "[Apollo] Failed to listen to namespace {}: {:?}", namespace, listen_res.unwrap());
                return Err(ApolloError::ClientInit(format!(
                    "Failed to listen to namespace: {}",
                    namespace
                )));
            }
        }
        drop(client_read);

        // Load initial config
        self.load_config().await?;

        // Spawn background listener task
        let task = tokio::spawn(async move {
            Self::listener_task(client, cache, namespace_map, shutdown_rx).await;
        });

        state.task = Some(task);
        state.shutdown_tx = Some(shutdown_tx);

        info!(target: "reth::apollo", "[Apollo] Started listening to configuration changes");
        Ok(())
    }

    // Background listener task
    async fn listener_task(
        client: Arc<RwLock<ApolloConfigClient>>,
        cache: Arc<RwLock<HashMap<String, JsonValue>>>,
        namespace_map: HashMap<String, String>,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!(target: "reth::apollo", "[Apollo] Stopping listener task");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {
                    // Poll for changes every second
                    let client_read = client.read().await;
                    for (_, namespace) in &namespace_map {
                        if let Some(change_event) = client_read.fetch_change_event() {
                            info!(target: "reth::apollo", "[Apollo] Configuration change detected for namespace {}: {:?}", namespace, change_event);

                            // After change event, fetch the updated config
                            match client_read.get_config_from_namespace("content", namespace) {
                                Some(config) => {
                                    info!(target: "reth::apollo", "[Apollo] Successfully fetched config for namespace {}: config_key={}", namespace, config.config_key);
                                    Self::update_cache_from_config(
                                        cache.clone(),
                                        namespace,
                                        &config.config_value,
                                    ).await;
                                }
                                None => {
                                    warn!(target: "reth::apollo", "[Apollo] get_config returned None for namespace {}. This may happen if the namespace format doesn't match.", namespace);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Parse YAML config and update cache with individual keys
    async fn update_cache_from_config(
        cache: Arc<RwLock<HashMap<String, JsonValue>>>,
        namespace: &str,
        config_value: &str,
    ) {
        match serde_yaml::from_str::<HashMap<String, JsonValue>>(config_value) {
            Ok(parsed_config) => {
                let mut cache_write = cache.write().await;
                info!(target: "reth::apollo", "[Apollo] Writing to cache for namespace {}: parsed {} keys", namespace, parsed_config.len());
                for (key, value) in parsed_config {
                    cache_write.insert(key, value);
                }
                drop(cache_write);
                info!(target: "reth::apollo", "[Apollo] Cache updated for namespace {}, {:?}", namespace, cache.read().await);
            }
            Err(e) => {
                error!(target: "reth::apollo", "[Apollo] Failed to parse YAML for namespace {}: {}", namespace, e);
            }
        }
    }

    // Stop listening and cleanup
    pub async fn stop_listening(&self) -> Result<(), ApolloError> {
        let mut state = self.listener_state.lock().await;
        if let Some(tx) = state.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }

        if let Some(task) = state.task.take() {
            task.abort();
        }

        info!(target: "reth::apollo", "[Apollo] Stopped listening to configuration changes");
        Ok(())
    }

    // Query cached configurations
    pub async fn get_cached_config(&self, namespace: &str, key: &str) -> Option<JsonValue> {
        let cache = self.cache.read().await;
        info!(target: "reth::apollo", "[Apollo] Getting cached config for namespace {}: key: {:?}", namespace, key);
        cache.get(&format!("{}:{}", namespace, key)).cloned()
    }

    // Get all cached configs for a namespace
    pub async fn get_namespace_configs(&self, namespace: &str) -> HashMap<String, JsonValue> {
        let cache = self.cache.read().await;
        cache
            .iter()
            .filter_map(|(k, v)| {
                if k.starts_with(&format!("{}:", namespace)) {
                    let key = k.strip_prefix(&format!("{}:", namespace))?;
                    Some((key.to_string(), v.clone()))
                } else {
                    None
                }
            })
            .collect()
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
