use crate::composite::store::CompositeStateStore;
use mptdb_common::{config::StateStoreConfig, error::Result};
use mptdb_traits::ss::StateStore;
use std::sync::Arc;

/// Create and initialize a [`CompositeStateStore`] with pruning enabled.
///
/// Returns `Arc` because the [`PruningManager`] holds an `Arc<dyn StateStore>`
/// reference back to the store, creating a shared-ownership relationship.
///
/// This mirrors the Go `ss.NewStateStore` factory which creates the composite
/// store and starts the pruning goroutine in one step.
pub fn new_state_store(
    config: &StateStoreConfig,
    home_dir: &str,
) -> Result<Arc<CompositeStateStore>> {
    let arc = Arc::new(CompositeStateStore::new(config, home_dir)?);

    // Start pruning: clone the Arc and upcast to Arc<dyn StateStore>.
    // start_pruning uses interior mutability (Mutex) so &self suffices.
    arc.start_pruning(Arc::clone(&arc) as Arc<dyn StateStore>);

    Ok(arc)
}

/// Resolve the backend database name.
///
/// In the Go implementation this selects between PebbleDB and RocksDB via
/// build tags. The Rust port only supports RocksDB, so this always returns
/// `"rocksdb"`.
pub fn resolve_backend(_name: &str) -> &'static str {
    "rocksdb"
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::config::{ReadMode, WriteMode};
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
            evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_new_state_store_creates_working_store() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();

        // Should be usable as a StateStore.
        assert_eq!(store.get_latest_version(), 0);
    }

    #[test]
    fn test_new_state_store_with_evm() {
        let dir = tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.write_mode = WriteMode::DualWrite;
        config.read_mode = ReadMode::EvmFirst;

        let store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();
        assert_eq!(store.get_latest_version(), 0);
    }

    #[test]
    fn test_new_state_store_with_pruning() {
        let dir = tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.keep_recent = 100;
        config.prune_interval_seconds = 600;

        let _store = new_state_store(&config, &dir.path().to_string_lossy()).unwrap();
        // Pruning is started internally; no panic means success.
    }

    #[test]
    fn test_resolve_backend() {
        assert_eq!(resolve_backend("pebbledb"), "rocksdb");
        assert_eq!(resolve_backend("rocksdb"), "rocksdb");
        assert_eq!(resolve_backend(""), "rocksdb");
    }
}
