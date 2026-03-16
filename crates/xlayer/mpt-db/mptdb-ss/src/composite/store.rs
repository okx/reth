use crate::{
    cosmos::CosmosStateStore, evm::store::EVMStateStore, evm_types::EVM_STORE_KEY,
    pruning::PruningManager,
};
use crossbeam_channel::Receiver;
use mptdb_common::{
    config::{ReadMode, StateStoreConfig, WriteMode},
    error::{MptDbError, Result},
    evm_keys::{parse_evm_key, EvmKeyKind},
};
use mptdb_engine::mvcc::db::MvccDatabase;
use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
use mptdb_traits::{iterator::DbIterator, ss::StateStore, types::SnapshotNode};
use parking_lot::Mutex;
use std::{path::Path, sync::Arc};
use tracing::error;

/// Routes operations between Cosmos and EVM backends based on WriteMode/ReadMode.
///
/// Both underlying stores implement [`StateStore`]; the composite itself also
/// implements [`StateStore`], transparently routing reads and writes according
/// to the configured modes.
pub struct CompositeStateStore {
    cosmos_store: CosmosStateStore,
    evm_store: Option<EVMStateStore>,
    pruning_manager: Mutex<Option<PruningManager>>,
    config: StateStoreConfig,
    /// Cached close result. Stores `Ok(())` or `Err(message)` after first close.
    close_result: Mutex<Option<std::result::Result<(), String>>>,
}

impl CompositeStateStore {
    /// Create a new composite state store.
    ///
    /// Opens the Cosmos MVCC database (always) and, when EVM is enabled by the
    /// config's write/read modes, the EVM sub-databases as well.
    pub fn new(config: &StateStoreConfig, home_dir: &str) -> Result<Self> {
        // Open Cosmos MVCC database.
        let db = MvccDatabase::open_db(config)
            .map_err(|e| MptDbError::Other(format!("failed to create cosmos MVCC DB: {e}")))?;

        // Wrap in Arc and initialize async writer if configured.
        let db_arc: Arc<MvccDatabase> = Arc::new(db);
        if config.async_write_buffer > 0 {
            db_arc
                .init_async_writer()
                .map_err(|e| MptDbError::Other(format!("init async writer: {e}")))?;
        }

        let cosmos_store = CosmosStateStore::new(db_arc);

        let mut cs = Self {
            cosmos_store,
            evm_store: None,
            pruning_manager: Mutex::new(None),
            config: config.clone(),
            close_result: Mutex::new(None),
        };

        if config.evm_enabled() {
            let evm_dir = if config.evm_db_directory.is_empty() {
                Path::new(home_dir).join("data").join("evm_ss").to_string_lossy().to_string()
            } else {
                config.evm_db_directory.clone()
            };

            match EVMStateStore::new(&evm_dir, config) {
                Ok(evm_store) => {
                    cs.evm_store = Some(evm_store);
                }
                Err(e) => {
                    let _ = cs.cosmos_store.close();
                    return Err(MptDbError::Other(format!("failed to create EVM store: {e}")));
                }
            }
        }

        Ok(cs)
    }

    /// Start the background pruning manager if keep_recent and prune_interval
    /// are both positive. Uses interior mutability so this can be called on
    /// `Arc<CompositeStateStore>` (needed by the factory function).
    pub fn start_pruning(&self, store_ref: Arc<dyn StateStore>) {
        if self.config.keep_recent > 0 && self.config.prune_interval_seconds > 0 {
            let mut pm = PruningManager::new(
                store_ref,
                self.config.keep_recent,
                self.config.prune_interval_seconds,
            );
            pm.start();
            *self.pruning_manager.lock() = Some(pm);
        }
    }

    /// Close the composite store. Idempotent: repeated calls return the cached
    /// result from the first close.
    pub fn close(&mut self) -> Result<()> {
        let mut guard = self.close_result.lock();
        if let Some(ref cached) = *guard {
            return cached.as_ref().map(|_| ()).map_err(|msg| MptDbError::Other(msg.clone()));
        }

        // Stop pruning first.
        if let Some(ref mut pm) = *self.pruning_manager.lock() {
            pm.stop();
        }

        let mut last_err_msg: Option<String> = None;

        // Close EVM store.
        if let Some(ref mut evm) = self.evm_store &&
            let Err(e) = evm.close()
        {
            error!(?e, "failed to close EVM store");
            last_err_msg = Some(e.to_string());
        }

        // Close Cosmos store.
        if let Err(e) = self.cosmos_store.close() {
            error!(?e, "failed to close Cosmos store");
            last_err_msg = Some(e.to_string());
        }

        let cached = match last_err_msg {
            Some(msg) => Err(msg),
            None => Ok(()),
        };
        *guard = Some(cached.clone());
        cached.map_err(MptDbError::Other)
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Return only changesets whose name matches [`EVM_STORE_KEY`].
fn filter_evm_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    changesets.iter().filter(|cs| cs.name == EVM_STORE_KEY).cloned().collect()
}

/// Strip EVM-typed key pairs from changesets named [`EVM_STORE_KEY`].
///
/// For changesets with a different name the changeset is kept as-is. For EVM
/// changesets, only pairs whose key parses to `Empty` or `Legacy` are retained
/// (these are non-EVM data that should stay in the Cosmos store). If all pairs
/// are stripped the changeset is omitted entirely.
fn strip_evm_from_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    let mut stripped = Vec::with_capacity(changesets.len());
    for cs in changesets {
        if cs.name != EVM_STORE_KEY {
            stripped.push(cs.clone());
            continue;
        }
        // For EVM changesets, filter out pairs that route to an EVM sub-DB.
        if let Some(ref changeset) = cs.changeset {
            let kept_pairs: Vec<KvPair> = changeset
                .pairs
                .iter()
                .filter(|pair| {
                    let (kind, _) = parse_evm_key(&pair.key);
                    kind == EvmKeyKind::Empty || kind == EvmKeyKind::Legacy
                })
                .cloned()
                .collect();
            if !kept_pairs.is_empty() {
                stripped.push(NamedChangeSet {
                    name: cs.name.clone(),
                    changeset: Some(ChangeSet { pairs: kept_pairs }),
                });
            }
        }
    }
    stripped
}

// ---------------------------------------------------------------------------
// StateStore implementation
// ---------------------------------------------------------------------------

impl StateStore for CompositeStateStore {
    fn get(&self, store_key: &str, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.config.read_mode {
            ReadMode::CosmosOnly => self.cosmos_store.get(store_key, version, key),
            ReadMode::EvmFirst => {
                if store_key == EVM_STORE_KEY &&
                    let Some(ref evm) = self.evm_store
                {
                    let val = evm.get(store_key, version, key)?;
                    if val.is_some() {
                        return Ok(val);
                    }
                }
                // Fallback to cosmos.
                self.cosmos_store.get(store_key, version, key)
            }
            ReadMode::SplitRead => {
                if store_key == EVM_STORE_KEY &&
                    let Some(ref evm) = self.evm_store
                {
                    return evm.get(store_key, version, key);
                }
                self.cosmos_store.get(store_key, version, key)
            }
        }
    }

    fn has(&self, store_key: &str, version: i64, key: &[u8]) -> Result<bool> {
        match self.config.read_mode {
            ReadMode::CosmosOnly => self.cosmos_store.has(store_key, version, key),
            ReadMode::EvmFirst => {
                if store_key == EVM_STORE_KEY &&
                    let Some(ref evm) = self.evm_store
                {
                    let found = evm.has(store_key, version, key)?;
                    if found {
                        return Ok(true);
                    }
                }
                self.cosmos_store.has(store_key, version, key)
            }
            ReadMode::SplitRead => {
                if store_key == EVM_STORE_KEY &&
                    let Some(ref evm) = self.evm_store
                {
                    return evm.has(store_key, version, key);
                }
                self.cosmos_store.has(store_key, version, key)
            }
        }
    }

    fn iterator(
        &self,
        store_key: &str,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        self.cosmos_store.iterator(store_key, version, start, end)
    }

    fn reverse_iterator(
        &self,
        store_key: &str,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        self.cosmos_store.reverse_iterator(store_key, version, start, end)
    }

    fn raw_iterate(
        &self,
        store_key: &str,
        f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool,
    ) -> Result<bool> {
        self.cosmos_store.raw_iterate(store_key, f)
    }

    fn apply_changeset_sync(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        if self.evm_store.is_none() || self.config.write_mode == WriteMode::CosmosOnly {
            return self.cosmos_store.apply_changeset_sync(version, changesets);
        }

        let evm_changesets = filter_evm_changesets(changesets);
        let cosmos_changesets;
        let cosmos_cs_ref: &[NamedChangeSet];

        if self.config.write_mode == WriteMode::SplitWrite {
            cosmos_changesets = strip_evm_from_changesets(changesets);
            cosmos_cs_ref = &cosmos_changesets;
        } else {
            // DualWrite: send all changesets to cosmos.
            cosmos_cs_ref = changesets;
        }

        self.cosmos_store
            .apply_changeset_sync(version, cosmos_cs_ref)
            .map_err(|e| MptDbError::Other(format!("cosmos store failed: {e}")))?;

        if !evm_changesets.is_empty() &&
            let Some(ref evm) = self.evm_store
        {
            evm.apply_changeset_sync(version, &evm_changesets)
                .map_err(|e| MptDbError::Other(format!("evm store failed: {e}")))?;
        }
        Ok(())
    }

    fn apply_changeset_async(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        if self.evm_store.is_none() || self.config.write_mode == WriteMode::CosmosOnly {
            return self.cosmos_store.apply_changeset_async(version, changesets);
        }

        let evm_changesets = filter_evm_changesets(changesets);
        let cosmos_changesets;
        let cosmos_cs_ref: &[NamedChangeSet];

        if self.config.write_mode == WriteMode::SplitWrite {
            cosmos_changesets = strip_evm_from_changesets(changesets);
            cosmos_cs_ref = &cosmos_changesets;
        } else {
            cosmos_cs_ref = changesets;
        }

        self.cosmos_store
            .apply_changeset_async(version, cosmos_cs_ref)
            .map_err(|e| MptDbError::Other(format!("cosmos store failed: {e}")))?;

        if !evm_changesets.is_empty() &&
            let Some(ref evm) = self.evm_store
        {
            evm.apply_changeset_async(version, &evm_changesets)
                .map_err(|e| MptDbError::Other(format!("evm store async enqueue failed: {e}")))?;
        }
        Ok(())
    }

    fn import(&self, version: i64, ch: Receiver<SnapshotNode>) -> Result<()> {
        if self.evm_store.is_none() || self.config.write_mode == WriteMode::CosmosOnly {
            return self.cosmos_store.import(version, ch);
        }

        let split_write = self.config.write_mode == WriteMode::SplitWrite;

        let (cosmos_tx, cosmos_rx) = crossbeam_channel::bounded::<SnapshotNode>(100);
        let (evm_tx, evm_rx) = crossbeam_channel::bounded::<SnapshotNode>(100);

        std::thread::scope(|s| {
            // Cosmos consumer thread.
            let cosmos_handle = s.spawn(|| self.cosmos_store.import(version, cosmos_rx));

            // EVM consumer thread.
            let evm_handle = s.spawn(|| {
                if let Some(ref evm) = self.evm_store {
                    evm.import(version, evm_rx)
                } else {
                    // Drain the channel even if no evm store.
                    for _ in evm_rx.iter() {}
                    Ok(())
                }
            });

            // Router: dispatch incoming nodes to cosmos and/or evm channels.
            for node in ch.iter() {
                let is_evm = node.store_key == EVM_STORE_KEY;
                if !is_evm || !split_write {
                    let _ = cosmos_tx.send(node.clone());
                }
                if is_evm {
                    let _ = evm_tx.send(node);
                }
            }
            drop(cosmos_tx);
            drop(evm_tx);

            let cosmos_err = cosmos_handle
                .join()
                .unwrap_or_else(|_| Err(MptDbError::Other("cosmos import thread panicked".into())));
            let evm_err = evm_handle
                .join()
                .unwrap_or_else(|_| Err(MptDbError::Other("evm import thread panicked".into())));

            cosmos_err?;
            evm_err
        })
    }

    fn prune(&self, version: i64) -> Result<()> {
        if let Some(ref evm) = self.evm_store &&
            let Err(e) = evm.prune(version)
        {
            error!(?e, "failed to prune EVM store");
        }
        self.cosmos_store.prune(version)
    }

    fn get_latest_version(&self) -> i64 {
        self.cosmos_store.get_latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.cosmos_store.set_latest_version(version)?;
        if self.config.write_mode != WriteMode::CosmosOnly &&
            let Some(ref evm) = self.evm_store &&
            let Err(e) = evm.set_latest_version(version)
        {
            error!(?e, "failed to set EVM store latest version");
        }
        Ok(())
    }

    fn get_earliest_version(&self) -> i64 {
        self.cosmos_store.get_earliest_version()
    }

    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        self.cosmos_store.set_earliest_version(version, ignore_version)?;
        if let Some(ref evm) = self.evm_store &&
            let Err(e) = evm.set_earliest_version(version, ignore_version)
        {
            error!(?e, "failed to set EVM store earliest version");
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        CompositeStateStore::close(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::evm_keys::NONCE_KEY_PREFIX;
    use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::tempdir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_config(
        dir: &std::path::Path,
        write_mode: WriteMode,
        read_mode: ReadMode,
    ) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
            evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
            keep_last_version: true,
            write_mode,
            read_mode,
            ..Default::default()
        }
    }

    fn make_changeset(store: &str, pairs: Vec<(&[u8], Option<&[u8]>)>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet {
            name: store.to_string(),
            changeset: Some(ChangeSet {
                pairs: pairs
                    .into_iter()
                    .map(|(k, v)| KvPair {
                        delete: v.is_none(),
                        key: k.to_vec(),
                        value: v.unwrap_or_default().to_vec(),
                    })
                    .collect(),
            }),
        }]
    }

    /// Build a nonce-type EVM key (prefix 0x0a || 20-byte address).
    fn make_nonce_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn test_addr() -> [u8; 20] {
        let mut addr = [0u8; 20];
        for (i, b) in addr.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        addr
    }

    fn open_composite(
        dir: &std::path::Path,
        write_mode: WriteMode,
        read_mode: ReadMode,
    ) -> CompositeStateStore {
        let cfg = make_config(dir, write_mode, read_mode);
        CompositeStateStore::new(&cfg, &dir.to_string_lossy()).unwrap()
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_composite_cosmos_only() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::CosmosOnly, ReadMode::CosmosOnly);

        // No EVM store should be created.
        assert!(store.evm_store.is_none());

        let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        let val = store.get("bank", 1, b"alice").unwrap();
        assert_eq!(val, Some(b"100".to_vec()));
    }

    #[test]
    fn test_composite_dual_write() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);
        assert!(store.evm_store.is_some());

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"42"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        // DualWrite: data should be in both stores.
        // EVM store should have it.
        let evm_val = store.evm_store.as_ref().unwrap().get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(evm_val, Some(b"42".to_vec()));

        // Cosmos store should also have it (dual write).
        let cosmos_val = store.cosmos_store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(cosmos_val, Some(b"42".to_vec()));

        // Composite get with EvmFirst should return from evm.
        let val = store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(val, Some(b"42".to_vec()));
    }

    #[test]
    fn test_composite_split_write() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::SplitWrite, ReadMode::SplitRead);
        assert!(store.evm_store.is_some());

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"99"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        // SplitWrite: EVM data should only be in EVM store, not cosmos.
        let evm_val = store.evm_store.as_ref().unwrap().get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(evm_val, Some(b"99".to_vec()));

        // Cosmos should NOT have the nonce key (it was stripped).
        let cosmos_val = store.cosmos_store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(cosmos_val, None);
    }

    #[test]
    fn test_composite_split_read_no_fallback() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::SplitRead);

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write to cosmos only (not via composite, simulating data only in cosmos).
        let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"50"))]);
        store.cosmos_store.apply_changeset_sync(1, &cs).unwrap();

        // SplitRead: EVM key queried against evm_store only — no fallback.
        // EVM store does not have this data.
        let val = store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_composite_evm_first_read() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::CosmosOnly, ReadMode::EvmFirst);

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write to cosmos only.
        let cs = make_changeset(EVM_STORE_KEY, vec![(nonce_key.as_slice(), Some(b"77"))]);
        store.cosmos_store.apply_changeset_sync(1, &cs).unwrap();

        // EvmFirst: try EVM (not found), then fallback to cosmos.
        let val = store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(val, Some(b"77".to_vec()));
    }

    #[test]
    fn test_composite_mixed_changeset() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::SplitWrite, ReadMode::SplitRead);

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Mixed changeset: bank + evm.
        let changesets = vec![
            NamedChangeSet {
                name: "bank".to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: b"alice".to_vec(),
                        value: b"100".to_vec(),
                    }],
                }),
            },
            NamedChangeSet {
                name: EVM_STORE_KEY.to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: nonce_key.clone(),
                        value: b"42".to_vec(),
                    }],
                }),
            },
        ];

        store.apply_changeset_sync(1, &changesets).unwrap();

        // Bank data should be in cosmos.
        let bank_val = store.get("bank", 1, b"alice").unwrap();
        assert_eq!(bank_val, Some(b"100".to_vec()));

        // EVM nonce should be in evm store.
        let evm_val = store.evm_store.as_ref().unwrap().get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(evm_val, Some(b"42".to_vec()));

        // EVM nonce should NOT be in cosmos (SplitWrite).
        let cosmos_evm = store.cosmos_store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(cosmos_evm, None);
    }

    #[test]
    fn test_composite_version_management() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

        assert_eq!(store.get_latest_version(), 0);
        assert_eq!(store.get_earliest_version(), 0);

        store.set_latest_version(10).unwrap();
        assert_eq!(store.get_latest_version(), 10);

        // EVM store should also have the latest version set (DualWrite != CosmosOnly).
        assert_eq!(store.evm_store.as_ref().unwrap().get_latest_version(), 10);

        store.set_earliest_version(5, false).unwrap();
        assert_eq!(store.get_earliest_version(), 5);
        assert_eq!(store.evm_store.as_ref().unwrap().get_earliest_version(), 5);
    }

    #[test]
    fn test_composite_prune_both() {
        let dir = tempdir().unwrap();
        let store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

        // Write v1 and v2 to a non-EVM store key so we exercise cosmos prune.
        let cs = make_changeset("bank", vec![(b"alice", Some(b"v1"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        let cs = make_changeset("bank", vec![(b"alice", Some(b"v2"))]);
        store.apply_changeset_sync(2, &cs).unwrap();

        // Prune version 1 — should succeed for both cosmos and evm stores.
        store.prune(1).unwrap();

        // Version 2 should still be available.
        let val = store.get("bank", 2, b"alice").unwrap();
        assert_eq!(val, Some(b"v2".to_vec()));
    }

    #[test]
    fn test_composite_close_idempotent() {
        let dir = tempdir().unwrap();
        let mut store = open_composite(dir.path(), WriteMode::DualWrite, ReadMode::EvmFirst);

        store.close().unwrap();
        // Second close returns the cached result.
        store.close().unwrap();
    }

    #[test]
    fn test_filter_evm_changesets() {
        let changesets = vec![
            NamedChangeSet {
                name: "bank".to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: b"alice".to_vec(),
                        value: b"100".to_vec(),
                    }],
                }),
            },
            NamedChangeSet {
                name: EVM_STORE_KEY.to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: b"evm_data".to_vec(),
                        value: b"val".to_vec(),
                    }],
                }),
            },
            NamedChangeSet {
                name: "staking".to_string(),
                changeset: Some(ChangeSet { pairs: vec![] }),
            },
        ];

        let filtered = filter_evm_changesets(&changesets);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, EVM_STORE_KEY);
    }

    #[test]
    fn test_strip_evm_from_changesets() {
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);
        // A legacy key (prefix 0x01) should be kept in cosmos.
        let legacy_key = vec![0x01, 0xaa, 0xbb];

        let changesets = vec![
            NamedChangeSet {
                name: "bank".to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![KvPair {
                        delete: false,
                        key: b"alice".to_vec(),
                        value: b"100".to_vec(),
                    }],
                }),
            },
            NamedChangeSet {
                name: EVM_STORE_KEY.to_string(),
                changeset: Some(ChangeSet {
                    pairs: vec![
                        // Nonce key → will be stripped (routes to EVM sub-DB).
                        KvPair { delete: false, key: nonce_key.clone(), value: b"42".to_vec() },
                        // Legacy key → will be kept in cosmos.
                        KvPair {
                            delete: false,
                            key: legacy_key.clone(),
                            value: b"legacy_val".to_vec(),
                        },
                    ],
                }),
            },
        ];

        let stripped = strip_evm_from_changesets(&changesets);
        assert_eq!(stripped.len(), 2); // bank + evm (with only legacy pair)
        assert_eq!(stripped[0].name, "bank");
        assert_eq!(stripped[1].name, EVM_STORE_KEY);
        let evm_pairs = &stripped[1].changeset.as_ref().unwrap().pairs;
        assert_eq!(evm_pairs.len(), 1);
        assert_eq!(evm_pairs[0].key, legacy_key);

        // If ALL pairs in an EVM changeset are EVM-typed, the changeset is omitted.
        let all_evm = vec![NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair { delete: false, key: nonce_key, value: b"42".to_vec() }],
            }),
        }];
        let stripped2 = strip_evm_from_changesets(&all_evm);
        assert!(stripped2.is_empty());
    }
}
