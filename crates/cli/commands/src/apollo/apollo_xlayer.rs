use once_cell::sync::OnceCell;
use reth_apollo::{client::NodeCommandFlags, ApolloHandler};
use reth_node_core::node_config::NodeConfig;
use std::sync::{Arc, RwLock};

// Global Apollo configuration instance (like op-geth)
static GLOBAL_APOLLO_CONFIG: OnceCell<ApolloConfigImpl> = OnceCell::new();

pub struct ApolloConfigImpl {
    pub node_config: Arc<RwLock<NodeConfig<ChainSpec>>>,
}

pub fn set_apollo_config(node_config: Arc<RwLock<NodeConfig<ChainSpec>>>) {
    GLOBAL_APOLLO_CONFIG.get_or_init(|| ApolloConfigImpl { node_config });
}

pub fn try_unsafe_get_apollo_config() -> Option<&'static ApolloConfigImpl> {
    GLOBAL_APOLLO_CONFIG.get()
}

pub struct RethConfigHandler;

impl ApolloHandler for RethConfigHandler {
    fn handle_config_change(
        &self,
        prefix: &str,
        flags: &HashMap<String, JsonValue>,
        key: &str,
        value: &ConfigChange,
    ) {
        // Your reth-specific logic here
    }

    fn load_config(&self, prefix: &str, flags: &HashMap<String, JsonValue>) {
        // Your reth-specific logic here
    }
}
