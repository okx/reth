use crate::types::{ApolloConfig, ApolloError};
use apollo_sdk::client::apollo_config_client::ApolloConfigClient;
use async_once_cell::OnceCell;
use moka::sync::Cache;
use serde_json::Value as JsonValue;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

const CACHE_EXPIRATION: Duration = Duration::from_secs(60);
/// Apollo client wrapper for reth
pub struct ApolloClient {
    pub inner: Arc<RwLock<ApolloConfigClient>>,
    pub config: ApolloConfig,
    pub namespace_map: HashMap<String, String>,
    pub cache: Arc<Cache<String, JsonValue>>,
    pub listener_state: Arc<Mutex<ListenerState>>,
}

pub struct ListenerState {
    task: Option<tokio::task::JoinHandle<()>>,
    shutdown_tx: Option<tokio::sync::mpsc::Sender<()>>,
}

/// Singleton instance
static INSTANCE: OnceCell<ApolloClient> = OnceCell::new();
const POLL_INTERVAL_SECS: u64 = 30;

impl std::fmt::Debug for ApolloClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApolloClient")
            .field("config", &self.config)
            .field("namespace_map", &self.namespace_map)
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
            cache: self.cache.clone(),
            listener_state: self.listener_state.clone(),
        }
    }
}

impl ApolloClient {
    /// Get singleton instance
    pub async fn new(
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
            cache: Arc::new(
                Cache::builder().max_capacity(1000).time_to_live(CACHE_EXPIRATION).build(),
            ),
            listener_state: Arc::new(Mutex::new(ListenerState { task: None, shutdown_tx: None })),
        })
    }

    pub fn get_instance() -> Result<ApolloClient, ApolloError> {
        INSTANCE
            .get()
            .cloned()
            .ok_or(ApolloError::ClientInit("Apollo client not initialized".to_string()))
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
        cache: Arc<Cache<String, JsonValue>>,
        namespace_map: HashMap<String, String>,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(POLL_INTERVAL_SECS));

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!(target: "reth::apollo", "[Apollo] Stopping listener task");
                    break;
                }
                _ = interval.tick() => {
                    // Fetch config on every interval
                    Self::fetch_and_update_configs(&client, &cache, &namespace_map).await;
                }
                _ = async {
                    // Check for change events
                    let client_read = client.read().await;
                    for (_, namespace) in &namespace_map {
                        if let Some(change_event) = client_read.fetch_change_event() {
                            info!(target: "reth::apollo", "[Apollo] Configuration change detected for namespace {}: {:?}", namespace, change_event);
                            Self::fetch_and_update_configs(&client, &cache, &namespace_map).await;
                            break;
                        }
                    }
                } => {}
            }
        }
    }

    async fn fetch_and_update_configs(
        client: &Arc<RwLock<ApolloConfigClient>>,
        cache: &Arc<Cache<String, JsonValue>>,
        namespace_map: &HashMap<String, String>,
    ) {
        let client_read = client.read().await;
        for (_, namespace) in namespace_map {
            if let Some(config) = client_read.get_config_from_namespace("content", namespace) {
                info!(target: "reth::apollo", "[Apollo] Fetched config for namespace {}: config_key={}", namespace, config.config_key);
                Self::update_cache_from_config(cache.clone(), namespace, &config.config_value)
                    .await;
            } else {
                warn!(target: "reth::apollo", "[Apollo] get_config returned None for namespace {}. This may happen if the namespace format doesn't match.", namespace);
            }
        }
    }

    /// Parse YAML config and update cache with individual keys
    async fn update_cache_from_config(
        cache: Arc<Cache<String, JsonValue>>,
        namespace: &str,
        config_value: &str,
    ) {
        match serde_yaml::from_str::<HashMap<String, JsonValue>>(config_value) {
            Ok(parsed_config) => {
                info!(target: "reth::apollo", "[Apollo] Writing to cache for namespace {}: parsed {} keys", namespace, parsed_config.len());
                for (key, value) in parsed_config {
                    cache.insert(key, value);
                }
                // DEBUG: Collect all cache entries for logging
                let mut cache_contents = HashMap::new();
                for (key, value) in cache.iter() {
                    cache_contents.insert(key.clone(), value.clone());
                }
                info!(target: "reth::apollo", "[Apollo] Cache updated for namespace {}, cache contents: {:?}", namespace, cache_contents);
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
        info!(target: "reth::apollo", "[Apollo] Getting cached config for namespace {}: key: {:?}", namespace, key);
        self.cache.get(key)
    }

    // Get all cached configs for a namespace
    pub async fn get_namespace_configs(&self, namespace: &str) -> HashMap<String, JsonValue> {
        self.cache
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
