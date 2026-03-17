use crate::evm::store::EVMStateStore;
use mptdb_common::{config::StateStoreConfig, error::Result};
use std::sync::Arc;

/// Create an EVM state store.
pub fn new_state_store(config: &StateStoreConfig, home_dir: &str) -> Result<Arc<EVMStateStore>> {
    let evm_dir = if config.db_directory.is_empty() {
        std::path::Path::new(home_dir).join("data").join("evm_ss").to_string_lossy().to_string()
    } else {
        config.db_directory.clone()
    };

    Ok(Arc::new(EVMStateStore::new(&evm_dir, config)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_traits::ss::StateStore;
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_new_state_store_creates_working_store() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();
        assert_eq!(store.get_latest_version(), 0);
    }
}
