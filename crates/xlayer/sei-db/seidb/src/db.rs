use seidb_common::{
    config::{StateCommitConfig, StateStoreConfig},
    error::Result,
};
use seidb_sc::composite::store::CompositeCommitStore;
use seidb_ss::{composite::store::CompositeStateStore, factory::new_state_store};
use std::sync::Arc;

/// Top-level entry point for the sei-db library.
/// Holds both SC (State Commitment) and SS (State Store) layers.
pub struct SeiDb {
    sc: CompositeCommitStore,
    ss: Option<Arc<CompositeStateStore>>,
    #[allow(dead_code)]
    home_dir: String,
}

/// Builder for constructing [`SeiDb`] with configuration.
pub struct SeiDbBuilder {
    home_dir: String,
    sc_config: StateCommitConfig,
    ss_config: Option<StateStoreConfig>,
}

impl SeiDb {
    /// Creates a new [`SeiDbBuilder`] with default SC config and no SS config.
    pub fn builder(home_dir: &str) -> SeiDbBuilder {
        SeiDbBuilder {
            home_dir: home_dir.to_string(),
            sc_config: StateCommitConfig::default(),
            ss_config: None,
        }
    }

    /// Convenience constructor: builds a [`SeiDb`] directly from configs.
    pub fn open(
        home_dir: &str,
        sc_config: StateCommitConfig,
        ss_config: Option<StateStoreConfig>,
    ) -> Result<Self> {
        let mut builder = Self::builder(home_dir).with_sc_config(sc_config);
        if let Some(ss) = ss_config {
            builder = builder.with_ss_config(ss);
        }
        builder.build()
    }

    /// Returns a reference to the SC (State Commitment) layer.
    pub fn sc(&self) -> &CompositeCommitStore {
        &self.sc
    }

    /// Returns a mutable reference to the SC (State Commitment) layer.
    pub fn sc_mut(&mut self) -> &mut CompositeCommitStore {
        &mut self.sc
    }

    /// Returns a reference to the SS (State Store) layer, if configured.
    pub fn ss(&self) -> Option<&Arc<CompositeStateStore>> {
        self.ss.as_ref()
    }

    /// Initializes the SC layer with the given module/store names.
    pub fn initialize(&mut self, initial_stores: &[String]) {
        self.sc.initialize(initial_stores);
    }

    /// Loads the specified version of the SC database.
    /// SS is already loaded during build so no additional work is needed.
    pub fn load_version(&mut self, target_version: i64) -> Result<()> {
        self.sc.load_version(target_version, false)
    }

    /// Closes the SC layer.
    /// SS close is handled by Arc drop and PruningManager stop.
    pub fn close(&mut self) -> Result<()> {
        self.sc.close()
    }

    /// Returns the current committed version from the SC layer.
    pub fn version(&self) -> i64 {
        self.sc.version()
    }
}

impl SeiDbBuilder {
    /// Sets the SC (State Commitment) configuration.
    pub fn with_sc_config(mut self, config: StateCommitConfig) -> Self {
        self.sc_config = config;
        self
    }

    /// Sets the SS (State Store) configuration.
    pub fn with_ss_config(mut self, config: StateStoreConfig) -> Self {
        self.ss_config = Some(config);
        self
    }

    /// Builds the [`SeiDb`] instance, opening SC and optionally SS backends.
    pub fn build(self) -> Result<SeiDb> {
        let sc = CompositeCommitStore::new(&self.home_dir, &self.sc_config);

        let ss = match self.ss_config {
            Some(ref cfg) if cfg.enable => Some(new_state_store(cfg, &self.home_dir)?),
            _ => None,
        };

        Ok(SeiDb { sc, ss, home_dir: self.home_dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn default_ss_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            enable: true,
            db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
            evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_seidb_builder() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();

        let db = SeiDb::builder(&home).with_sc_config(StateCommitConfig::default()).build();
        assert!(db.is_ok());

        let db = db.unwrap();
        assert_eq!(db.version(), 0);
    }

    #[test]
    fn test_seidb_open_close() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();

        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        // load_version on a fresh (unopened) memiavl will fail because no DB exists,
        // but version 0 is the default state before any load.
        assert_eq!(db.version(), 0);
        let close_result = db.close();
        assert!(close_result.is_ok());
    }

    #[test]
    fn test_seidb_sc_only() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();

        let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        // No SS config provided -> ss() should be None.
        assert!(db.ss().is_none());
        assert!(db.sc().version() == 0);
    }

    #[test]
    fn test_seidb_with_ss() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let ss_config = default_ss_config(dir.path());

        let db = SeiDb::open(&home, StateCommitConfig::default(), Some(ss_config)).unwrap();
        // SS config provided and enabled -> ss() should be Some.
        assert!(db.ss().is_some());
    }

    #[test]
    fn test_seidb_version() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();

        let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        // Fresh database should report version 0.
        assert_eq!(db.version(), 0);
    }
}
