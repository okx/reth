use crate::{flatkv, memiavl::commit_store::MemiavlCommitStore};
use seidb_common::{
    config::{StateCommitConfig, WriteMode},
    error::{Result, SeiDbError},
    evm_keys::{parse_evm_key, EvmKeyKind},
};
use seidb_proto::{ChangeSet, CommitId, CommitInfo, NamedChangeSet, StoreInfo, TreeNameUpgrade};
use seidb_traits::sc::{CommitKvStore, Exporter, Importer};
use std::path::{Path, PathBuf};

/// Module name for the EVM store in named change sets.
const EVM_STORE_NAME: &str = "evm";

/// Coordinates between Cosmos (memiavl) and EVM (flatkv) commit store backends,
/// routing writes based on the configured [`WriteMode`].
///
/// Mirrors the Go `CompositeCommitStore` in `sei-db/state_db/sc/composite/store.go`.
pub struct CompositeCommitStore {
    pub(crate) cosmos_committer: MemiavlCommitStore,
    pub(crate) evm_committer: Option<flatkv::store::CommitStore>,
    #[allow(dead_code)]
    pub(crate) home_dir: String,
    pub(crate) config: StateCommitConfig,
}

impl CompositeCommitStore {
    /// Creates a new composite commit store.
    ///
    /// The store is NOT opened yet -- call [`load_version`] to open and initialize
    /// the backing databases. This matches the `memiavl::MemiavlCommitStore::new` pattern.
    ///
    /// The FlatKV backend is only created when `WriteMode` is `DualWrite` or `SplitWrite`.
    pub fn new(home_dir: &str, config: &StateCommitConfig) -> Self {
        let cosmos_committer = MemiavlCommitStore::new(home_dir, config.memiavl.clone());

        let evm_committer = match config.write_mode {
            WriteMode::DualWrite | WriteMode::SplitWrite => {
                let flatkv_dir = flatkv_path(home_dir);
                Some(flatkv::store::CommitStore::new(
                    flatkv_dir.to_str().unwrap_or(""),
                    config.flatkv.clone(),
                ))
            }
            WriteMode::CosmosOnly => None,
        };

        Self {
            cosmos_committer,
            evm_committer,
            home_dir: home_dir.to_string(),
            config: config.clone(),
        }
    }

    /// Initializes the store with the given module/store names.
    pub fn initialize(&mut self, initial_stores: &[String]) {
        self.cosmos_committer.initialize(initial_stores);
    }

    /// Sets the initial version for the store (delegates to cosmos backend).
    pub fn set_initial_version(&mut self, version: i64) -> Result<()> {
        self.cosmos_committer.set_initial_version(version)
    }

    /// Loads the specified version of the database.
    ///
    /// Opens the Cosmos (memiavl) backend at `target_version` and, if the EVM
    /// backend is configured, opens it at the same version.
    pub fn load_version(&mut self, target_version: i64, read_only: bool) -> Result<()> {
        self.cosmos_committer.load_version(target_version, read_only)?;

        if let Some(ref mut evm) = self.evm_committer {
            evm.load_version(target_version)?;
        }

        Ok(())
    }

    /// Closes all backends, aggregating errors from both.
    pub fn close(&mut self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        if let Err(e) = self.cosmos_committer.close() {
            errors.push(format!("failed to close cosmos: {e}"));
        }

        if let Some(ref mut evm) = self.evm_committer &&
            let Err(e) = evm.close()
        {
            errors.push(format!("failed to close FlatKV: {e}"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(SeiDbError::Other(errors.join("; ")))
        }
    }

    /// Applies change sets to the appropriate backends based on configured [`WriteMode`].
    ///
    /// - `CosmosOnly`: all changesets go to cosmos only.
    /// - `DualWrite`: all changesets go to cosmos; EVM-named changesets also go to flatkv.
    /// - `SplitWrite`: EVM changesets (with EVM-typed keys stripped) go to cosmos; full EVM
    ///   changesets go to flatkv; non-EVM changesets go to cosmos as-is.
    pub fn apply_change_sets(&mut self, changesets: &[NamedChangeSet]) -> Result<()> {
        if changesets.is_empty() {
            return Ok(());
        }

        let evm_changesets = filter_evm_changesets(changesets);

        match self.config.write_mode {
            WriteMode::CosmosOnly => {
                // All data goes to cosmos
                self.cosmos_committer.apply_change_sets(changesets)?;
            }
            WriteMode::DualWrite => {
                // All data goes to cosmos, EVM data also goes to flatkv.
                // Run both in parallel since they operate on independent
                // data structures (MemIAVL trees vs RocksDB instances).
                if let Some(ref mut evm) = self.evm_committer &&
                    !evm_changesets.is_empty()
                {
                    let cosmos = &mut self.cosmos_committer;
                    let mut cosmos_err: Result<()> = Ok(());
                    let mut evm_err: Result<()> = Ok(());
                    std::thread::scope(|s| {
                        let cosmos_handle = s.spawn(|| cosmos.apply_change_sets(changesets));
                        evm_err = evm.apply_change_sets(&evm_changesets);
                        cosmos_err = cosmos_handle.join().expect("cosmos apply panicked");
                    });
                    cosmos_err?;
                    evm_err?;
                } else {
                    self.cosmos_committer.apply_change_sets(changesets)?;
                }
            }
            WriteMode::SplitWrite => {
                // Strip EVM-typed keys from cosmos; EVM data goes to flatkv
                let cosmos_changesets = strip_evm_from_changesets(changesets);
                if !cosmos_changesets.is_empty() {
                    self.cosmos_committer.apply_change_sets(&cosmos_changesets)?;
                }
                if let Some(ref mut evm) = self.evm_committer &&
                    !evm_changesets.is_empty()
                {
                    evm.apply_change_sets(&evm_changesets)?;
                }
            }
        }

        Ok(())
    }

    /// Applies tree name upgrades. Only applicable to the Cosmos (memiavl) backend;
    /// FlatKV does not have tree upgrades.
    pub fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()> {
        self.cosmos_committer.apply_upgrades(upgrades)
    }

    /// Commits the current state to all active backends and returns the new version.
    ///
    /// If both backends are active, runs cosmos and EVM commits in parallel
    /// then verifies they produce the same version number.
    pub fn commit(&mut self) -> Result<i64> {
        if let Some(ref mut evm) = self.evm_committer {
            let cosmos = &mut self.cosmos_committer;
            let mut cosmos_result: Result<i64> = Ok(0);
            let mut evm_result: Result<i64> = Ok(0);
            std::thread::scope(|s| {
                let cosmos_handle = s.spawn(|| cosmos.commit());
                evm_result = evm.commit();
                cosmos_result = cosmos_handle.join().expect("cosmos commit panicked");
            });
            let cosmos_version = cosmos_result?;
            let evm_version = evm_result?;
            if cosmos_version != evm_version {
                return Err(SeiDbError::Other(format!(
                    "cosmos and EVM version mismatch after commit: cosmos={cosmos_version}, evm={evm_version}"
                )));
            }
            Ok(cosmos_version)
        } else {
            self.cosmos_committer.commit()
        }
    }

    /// Returns the current committed version (from the cosmos backend).
    pub fn version(&self) -> i64 {
        self.cosmos_committer.version()
    }

    /// Returns the latest version available on disk.
    pub fn get_latest_version(&self) -> Result<i64> {
        self.cosmos_committer.get_latest_version()
    }

    /// Returns the earliest version available on disk.
    pub fn get_earliest_version(&self) -> Result<i64> {
        self.cosmos_committer.get_earliest_version()
    }

    /// Rolls back both backends to the specified version.
    pub fn rollback(&mut self, target_version: i64) -> Result<()> {
        self.cosmos_committer.rollback(target_version)?;

        if let Some(ref mut evm) = self.evm_committer {
            evm.rollback(target_version)?;
        }

        Ok(())
    }

    /// Returns the commit info for the current working (uncommitted) state.
    ///
    /// When the EVM (FlatKV) backend is active, its root hash is appended as an
    /// additional `StoreInfo` entry so that the combined hash covers both the
    /// Cosmos memiavl trees and the FlatKV state.
    pub fn working_commit_info(&self) -> CommitInfo {
        let mut info = self.cosmos_committer.working_commit_info();
        if let Some(ref evm) = self.evm_committer {
            info.store_infos.push(StoreInfo {
                name: "evm_flatkv".to_string(),
                commit_id: Some(CommitId { version: evm.version(), hash: evm.root_hash() }),
            });
        }
        info
    }

    /// Returns the commit info for the last committed version.
    ///
    /// When the EVM (FlatKV) backend is active, its root hash is appended as an
    /// additional `StoreInfo` entry so that the combined hash covers both the
    /// Cosmos memiavl trees and the FlatKV state.
    pub fn last_commit_info(&self) -> CommitInfo {
        let mut info = self.cosmos_committer.last_commit_info();
        if let Some(ref evm) = self.evm_committer {
            info.store_infos.push(StoreInfo {
                name: "evm_flatkv".to_string(),
                commit_id: Some(CommitId { version: evm.version(), hash: evm.root_hash() }),
            });
        }
        info
    }

    /// Returns the child store by module name.
    ///
    /// Retrieves the named tree from the cosmos (memiavl) backend and returns
    /// an O(1) CoW clone as a `CommitKvStore` trait object.
    pub fn get_child_store_by_name(&self, name: &str) -> Option<Box<dyn CommitKvStore>> {
        let tree_ref = self.cosmos_committer.get_child_store_by_name(name)?;
        let cloned = tree_ref.snapshot_copy();
        Some(Box::new(cloned))
    }

    /// Creates an importer for state sync at the given version.
    ///
    /// The cosmos (memiavl) importer receives all nodes — it needs the full tree
    /// for root hash computation. The EVM (flatkv) importer is currently `None`:
    /// EVM leaf nodes are included in the memiavl tree hash, and FlatKV can be
    /// rebuilt separately.
    pub fn create_importer(&self, version: i64) -> Result<Box<dyn Importer>> {
        let cosmos_importer = self.cosmos_committer.importer(version)?;

        // EVM importer is None for now: EVM nodes go to memiavl (for tree hash
        // computation). FlatKV can be rebuilt separately from the memiavl snapshot.
        // This matches the Go approach where evmImporter is only populated when
        // the EVM committer explicitly supports snapshot import.
        let evm_importer: Option<Box<dyn Importer>> = None;

        let importer =
            crate::composite::importer::SnapshotImporter::new(cosmos_importer, evm_importer);
        Ok(Box::new(importer))
    }

    /// Creates an exporter for state sync at the given version.
    ///
    /// Delegates to the Cosmos (memiavl) commit store which exports all named
    /// trees sequentially in post-order.
    pub fn create_exporter(&self, version: i64) -> Result<Box<dyn Exporter>> {
        self.cosmos_committer.exporter(version)
    }
}

/// Returns the path to the FlatKV data directory under the given home.
fn flatkv_path(home_dir: &str) -> PathBuf {
    Path::new(home_dir).join("data").join("flatkv")
}

/// Filters changesets to only those belonging to the EVM store.
fn filter_evm_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    changesets.iter().filter(|cs| cs.name == EVM_STORE_NAME).cloned().collect()
}

/// Strips EVM-typed keys from EVM-named changesets, keeping only Empty and Legacy
/// keys that should remain in the cosmos backend. Non-EVM changesets pass through
/// unchanged. Changesets that end up empty are dropped.
fn strip_evm_from_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    changesets
        .iter()
        .map(|cs| {
            if cs.name != EVM_STORE_NAME {
                return cs.clone();
            }
            // Filter out EVM-typed keys from the EVM changeset, keeping only
            // Empty and Legacy keys (which still belong in cosmos/memiavl).
            let filtered_pairs = cs.changeset.as_ref().map(|c| {
                c.pairs
                    .iter()
                    .filter(|p| {
                        let (kind, _) = parse_evm_key(&p.key);
                        matches!(kind, EvmKeyKind::Empty | EvmKeyKind::Legacy)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
            NamedChangeSet {
                name: cs.name.clone(),
                changeset: filtered_pairs.map(|pairs| ChangeSet { pairs }),
            }
        })
        .filter(|cs| cs.changeset.as_ref().is_some_and(|c| !c.pairs.is_empty()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::evm_keys::{NONCE_KEY_PREFIX, STATE_KEY_PREFIX};
    use seidb_proto::KvPair;

    fn default_config(write_mode: WriteMode) -> StateCommitConfig {
        StateCommitConfig { write_mode, ..Default::default() }
    }

    fn make_evm_changeset(pairs: Vec<KvPair>) -> NamedChangeSet {
        NamedChangeSet { name: EVM_STORE_NAME.to_string(), changeset: Some(ChangeSet { pairs }) }
    }

    fn make_cosmos_changeset(name: &str, pairs: Vec<KvPair>) -> NamedChangeSet {
        NamedChangeSet { name: name.to_string(), changeset: Some(ChangeSet { pairs }) }
    }

    fn kv(key: Vec<u8>, value: Vec<u8>) -> KvPair {
        KvPair { delete: false, key, value }
    }

    fn nonce_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut k = vec![NONCE_KEY_PREFIX];
        k.extend_from_slice(addr);
        k
    }

    fn storage_key(addr: &[u8; 20], slot: &[u8; 32]) -> Vec<u8> {
        let mut k = vec![STATE_KEY_PREFIX];
        k.extend_from_slice(addr);
        k.extend_from_slice(slot);
        k
    }

    fn legacy_key() -> Vec<u8> {
        vec![0x01, 0xaa, 0xbb, 0xcc]
    }

    #[test]
    fn test_composite_new_cosmos_only() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_composite_cosmos", &cfg);
        assert!(store.evm_committer.is_none());
        assert_eq!(store.version(), 0);
    }

    #[test]
    fn test_composite_new_dual_write() {
        let cfg = default_config(WriteMode::DualWrite);
        let store = CompositeCommitStore::new("/tmp/test_composite_dual", &cfg);
        assert!(store.evm_committer.is_some());
    }

    #[test]
    fn test_composite_new_split_write() {
        let cfg = default_config(WriteMode::SplitWrite);
        let store = CompositeCommitStore::new("/tmp/test_composite_split", &cfg);
        assert!(store.evm_committer.is_some());
    }

    #[test]
    fn test_filter_evm_changesets() {
        let addr = [1u8; 20];
        let cs = vec![
            make_cosmos_changeset("bank", vec![kv(b"balance".to_vec(), b"100".to_vec())]),
            make_evm_changeset(vec![kv(nonce_key(&addr), vec![1])]),
        ];
        let filtered = filter_evm_changesets(&cs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, EVM_STORE_NAME);
    }

    #[test]
    fn test_strip_evm_from_changesets_keeps_legacy() {
        let addr = [1u8; 20];
        let slot = [2u8; 32];
        let cs = vec![
            make_cosmos_changeset("bank", vec![kv(b"balance".to_vec(), b"100".to_vec())]),
            make_evm_changeset(vec![
                kv(nonce_key(&addr), vec![1]),          // EVM-typed -> stripped
                kv(storage_key(&addr, &slot), vec![2]), // EVM-typed -> stripped
                kv(legacy_key(), vec![3]),              // Legacy -> kept
            ]),
        ];
        let stripped = strip_evm_from_changesets(&cs);
        assert_eq!(stripped.len(), 2); // bank + evm (with only legacy)
                                       // bank unchanged
        assert_eq!(stripped[0].name, "bank");
        // evm has only the legacy key
        assert_eq!(stripped[1].name, EVM_STORE_NAME);
        let pairs = &stripped[1].changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].key, legacy_key());
    }

    #[test]
    fn test_strip_evm_drops_empty_changeset() {
        let addr = [1u8; 20];
        // All keys are EVM-typed, so the EVM changeset should be dropped entirely
        let cs = vec![make_evm_changeset(vec![kv(nonce_key(&addr), vec![1])])];
        let stripped = strip_evm_from_changesets(&cs);
        assert!(stripped.is_empty());
    }

    #[test]
    fn test_composite_apply_cosmos_only() {
        // In CosmosOnly mode, apply_change_sets sends everything to cosmos.
        // We only verify it does not panic; deeper testing requires opened DBs.
        let cfg = default_config(WriteMode::CosmosOnly);
        let mut store = CompositeCommitStore::new("/tmp/test_apply_cosmos_only", &cfg);
        // Empty changesets should succeed even without an opened DB
        let result = store.apply_change_sets(&[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_apply_dual_write_empty() {
        let cfg = default_config(WriteMode::DualWrite);
        let mut store = CompositeCommitStore::new("/tmp/test_apply_dual_empty", &cfg);
        let result = store.apply_change_sets(&[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_apply_split_write_empty() {
        let cfg = default_config(WriteMode::SplitWrite);
        let mut store = CompositeCommitStore::new("/tmp/test_apply_split_empty", &cfg);
        let result = store.apply_change_sets(&[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_version_default() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_version_default", &cfg);
        assert_eq!(store.version(), 0);
    }

    #[test]
    fn test_composite_working_commit_info_default() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_commit_info", &cfg);
        let info = store.working_commit_info();
        assert_eq!(info.version, 0);
    }

    #[test]
    fn test_composite_last_commit_info_default() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_last_commit_info", &cfg);
        let info = store.last_commit_info();
        assert_eq!(info.version, 0);
    }

    #[test]
    fn test_composite_upgrades_cosmos_only() {
        // Upgrades should only go to cosmos. Without an opened DB this returns an error.
        let cfg = default_config(WriteMode::CosmosOnly);
        let mut store = CompositeCommitStore::new("/tmp/test_upgrades", &cfg);
        // Empty upgrades should succeed
        let result = store.apply_upgrades(&[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_close_no_panic() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let mut store = CompositeCommitStore::new("/tmp/test_close_nopanic", &cfg);
        // Close without having opened should succeed
        let result = store.close();
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_close_dual_no_panic() {
        let cfg = default_config(WriteMode::DualWrite);
        let mut store = CompositeCommitStore::new("/tmp/test_close_dual_nopanic", &cfg);
        let result = store.close();
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_child_store_returns_none() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_child_store", &cfg);
        assert!(store.get_child_store_by_name("bank").is_none());
        assert!(store.get_child_store_by_name("evm").is_none());
    }

    #[test]
    fn test_composite_create_importer() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new(home, &cfg);

        // create_importer should succeed now
        let mut importer = store.create_importer(5).unwrap();

        // Import a module
        importer.add_module("bank").unwrap();
        importer.add_node(&seidb_traits::sc::ScSnapshotNode {
            key: b"key1".to_vec(),
            value: b"val1".to_vec(),
            version: 5,
            height: 0,
        });
        importer.close().unwrap();

        // Verify snapshot was created
        let commit_path = seidb_common::path::get_commit_store_path(std::path::Path::new(home));
        let version = seidb_common::snapshot_dir::current_version(&commit_path).unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn test_composite_create_importer_invalid_version() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_importer_invalid", &cfg);
        assert!(store.create_importer(0).is_err());
        assert!(store.create_importer(-1).is_err());
    }

    #[test]
    fn test_composite_create_exporter_not_implemented() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_exporter", &cfg);
        assert!(store.create_exporter(1).is_err());
    }

    #[test]
    fn test_composite_commit_without_open_errors() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let mut store = CompositeCommitStore::new("/tmp/test_commit_err", &cfg);
        // Commit without opening the DB should error
        assert!(store.commit().is_err());
    }

    #[test]
    fn test_composite_set_initial_version_without_open_errors() {
        let cfg = default_config(WriteMode::CosmosOnly);
        let mut store = CompositeCommitStore::new("/tmp/test_init_ver_err", &cfg);
        assert!(store.set_initial_version(1).is_err());
    }

    #[test]
    fn test_composite_commit_info_includes_evm() {
        // DualWrite mode: commit_info should include both cosmos stores and evm_flatkv.
        let cfg = default_config(WriteMode::DualWrite);
        let store = CompositeCommitStore::new("/tmp/test_commit_info_evm", &cfg);
        assert!(store.evm_committer.is_some(), "DualWrite should have evm_committer");

        let working = store.working_commit_info();
        let has_evm = working.store_infos.iter().any(|si| si.name == "evm_flatkv");
        assert!(has_evm, "working_commit_info should contain evm_flatkv store info");

        // Verify the evm_flatkv entry has a valid CommitId.
        let evm_si = working.store_infos.iter().find(|si| si.name == "evm_flatkv").unwrap();
        let cid = evm_si.commit_id.as_ref().expect("commit_id should be Some");
        assert_eq!(cid.version, 0, "unopened store should report version 0");
        assert_eq!(cid.hash.len(), 32, "root_hash should be 32 bytes (Blake3)");

        let last = store.last_commit_info();
        let has_evm_last = last.store_infos.iter().any(|si| si.name == "evm_flatkv");
        assert!(has_evm_last, "last_commit_info should contain evm_flatkv store info");
    }

    #[test]
    fn test_composite_commit_info_cosmos_only() {
        // CosmosOnly mode: commit_info should NOT include evm_flatkv.
        let cfg = default_config(WriteMode::CosmosOnly);
        let store = CompositeCommitStore::new("/tmp/test_commit_info_cosmos_only", &cfg);
        assert!(store.evm_committer.is_none());

        let working = store.working_commit_info();
        let has_evm = working.store_infos.iter().any(|si| si.name == "evm_flatkv");
        assert!(!has_evm, "CosmosOnly working_commit_info should not contain evm_flatkv");

        let last = store.last_commit_info();
        let has_evm_last = last.store_infos.iter().any(|si| si.name == "evm_flatkv");
        assert!(!has_evm_last, "CosmosOnly last_commit_info should not contain evm_flatkv");
    }
}
