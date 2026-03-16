use seidb_common::{
    config::{StateCommitConfig, StateStoreConfig},
    error::{Result, SeiDbError},
    path::resolve_sc_path,
};
use seidb_sc::mpt::{MptCommitStore, MptCommitter};
use seidb_ss::{composite::store::CompositeStateStore, factory::new_state_store};
use std::{path::Path, sync::Arc};

/// Top-level entry point for the sei-db library.
/// Holds both SC (State Commitment via MPT) and SS (State Store) layers.
pub struct SeiDb {
    sc: MptCommitStore,
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
    pub fn sc(&self) -> &MptCommitStore {
        &self.sc
    }

    /// Returns a mutable reference to the SC (State Commitment) layer.
    pub fn sc_mut(&mut self) -> &mut MptCommitStore {
        &mut self.sc
    }

    /// Returns a reference to the SS (State Store) layer, if configured.
    pub fn ss(&self) -> Option<&Arc<CompositeStateStore>> {
        self.ss.as_ref()
    }

    /// Initializes the SC layer with the given module/store names.
    /// In the MPT architecture this is a no-op, retained for compatibility.
    pub fn initialize(&mut self, _initial_stores: &[String]) {
        // no-op: MPT does not use named stores
    }

    /// Loads the latest committed version of the SC database.
    ///
    /// Only `target_version == 0` is supported (meaning "load latest").
    /// For rollback semantics, use `sc_mut().rollback(target)` then `load_version(0)`.
    pub fn load_version(&mut self, target_version: i64) -> Result<()> {
        if target_version != 0 {
            return Err(SeiDbError::Other(format!(
                "MPT SC only supports loading latest version (target_version=0); \
                 got target_version={target_version}. Use rollback(target) then load_version(0) instead."
            )));
        }
        self.sc.load_version()
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
        if !self.sc_config.enable {
            return Err(SeiDbError::Other(
                "state-commit layer disabled; MPT SC is required".to_string(),
            ));
        }

        let sc_path = resolve_sc_path(Path::new(&self.home_dir), &self.sc_config);
        let sc = MptCommitStore::open(&sc_path, false)?;

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

    /// T2.1: SeiDb::build(default config) -> sc is MptCommitStore
    #[test]
    fn t2_1_build_default_config() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let db = SeiDb::builder(&home).with_sc_config(StateCommitConfig::default()).build();
        assert!(db.is_ok());
    }

    /// T2.2: version() on fresh DB == 0
    #[test]
    fn t2_2_fresh_version() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        assert_eq!(db.version(), 0);
    }

    /// T2.3: initialize() is no-op, does not panic
    #[test]
    fn t2_3_initialize_noop() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        db.initialize(&["bank".to_string(), "staking".to_string()]);
        assert_eq!(db.version(), 0);
    }

    /// T2.4: load_version(0) succeeds (load latest)
    #[test]
    fn t2_4_load_version_zero() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        assert!(db.load_version(0).is_ok());
    }

    /// T2.5: load_version(3) returns Err
    #[test]
    fn t2_5_load_version_nonzero() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        let result = db.load_version(3);
        assert!(result.is_err());
    }

    /// T2.6: close() works
    #[test]
    fn t2_6_close() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        assert!(db.close().is_ok());
    }

    /// T2.7: SS config does not affect ss() construction
    #[test]
    fn t2_7_with_ss() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let ss_config = default_ss_config(dir.path());
        let db = SeiDb::open(&home, StateCommitConfig::default(), Some(ss_config)).unwrap();
        assert!(db.ss().is_some());
    }

    /// T2.8: sc()/sc_mut() directly expose MptCommitStore
    #[test]
    fn t2_8_sc_accessors() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        // sc() returns &MptCommitStore
        let _: &MptCommitStore = db.sc();
        // sc_mut() returns &mut MptCommitStore
        let _: &mut MptCommitStore = db.sc_mut();
    }

    /// T2.9: sc_config.enable=false -> build returns Err
    #[test]
    fn t2_9_sc_disabled() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let config = StateCommitConfig { enable: false, ..Default::default() };
        let result = SeiDb::open(&home, config, None);
        assert!(result.is_err());
    }

    /// T2.10: trait method calls work via `use MptCommitter`
    #[test]
    fn t2_10_trait_method_resolution() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        // These calls go through MptCommitter trait methods
        assert_eq!(db.sc().version(), 0);
        assert!(db.sc_mut().load_version().is_ok());
        assert!(db.sc_mut().close().is_ok());
    }

    /// T2.11: commit() returns (i64, B256), not just i64
    #[test]
    fn t2_11_commit_returns_tuple() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let mut db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        db.sc_mut().apply_bundle_state(&revm_database::BundleState::default()).unwrap();
        let (version, _state_root) = db.sc_mut().commit().unwrap();
        assert_eq!(version, 1);
    }

    /// SS=None -> ss() is None
    #[test]
    fn test_sc_only() {
        let dir = tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let db = SeiDb::open(&home, StateCommitConfig::default(), None).unwrap();
        assert!(db.ss().is_none());
    }
}
