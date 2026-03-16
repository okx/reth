use crate::evm_types::{all_evm_store_types, store_type_name, sub_db_config, EVM_STORE_KEY};
use crossbeam_channel::Receiver;
use mptdb_common::{
    config::StateStoreConfig,
    error::{MptDbError, Result},
    evm_keys::{parse_evm_key, EvmKeyKind},
};
use mptdb_engine::mvcc::db::MvccDatabase;
use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
use mptdb_traits::{iterator::DbIterator, ss::StateStore, types::SnapshotNode};
use std::{collections::HashMap, path::Path};

const IMPORT_BUFFER_SIZE: usize = 10000;

/// Manages 5 independent MVCC sub-databases (one per EVM key type) and
/// implements [`StateStore`]. Key routing is handled via [`parse_evm_key`],
/// so callers pass raw EVM keys and the store dispatches to the correct sub-DB.
pub struct EVMStateStore {
    sub_dbs: HashMap<EvmKeyKind, Box<dyn StateStore>>,
}

impl EVMStateStore {
    /// Opens 5 MVCC sub-databases under `dir`, one per EVM store type.
    pub fn new(dir: &str, config: &StateStoreConfig) -> Result<Self> {
        let mut sub_dbs =
            HashMap::<EvmKeyKind, Box<dyn StateStore>>::with_capacity(all_evm_store_types().len());

        for store_type in all_evm_store_types() {
            let sub_dir = Path::new(dir).join(store_type_name(store_type));
            let mut sub_config = sub_db_config(config);
            sub_config.db_directory = sub_dir.to_string_lossy().to_string();

            let db = MvccDatabase::open_db(&sub_config).map_err(|e| {
                MptDbError::Other(format!(
                    "failed to open EVM MVCC DB for {}: {}",
                    store_type_name(store_type),
                    e
                ))
            })?;
            sub_dbs.insert(store_type, Box::new(db));
        }

        Ok(Self { sub_dbs })
    }

    /// Parse an EVM key and return the routed sub-DB kind and stripped key.
    /// Returns `None` for empty keys.
    fn route_key(&self, key: &[u8]) -> Option<(EvmKeyKind, Vec<u8>)> {
        let (kind, stripped) = parse_evm_key(key);
        if kind == EvmKeyKind::Empty {
            return None;
        }
        Some((kind, stripped.to_vec()))
    }

    /// Groups changeset pairs by EVM key kind, stripping prefixes.
    /// Only processes changesets named [`EVM_STORE_KEY`].
    fn group_by_sub_type(changesets: &[NamedChangeSet]) -> HashMap<EvmKeyKind, Vec<KvPair>> {
        let mut grouped = HashMap::<EvmKeyKind, Vec<KvPair>>::new();
        for cs in changesets {
            if cs.name != EVM_STORE_KEY {
                continue;
            }
            if let Some(ref changeset) = cs.changeset {
                for pair in &changeset.pairs {
                    let (kind, stripped) = parse_evm_key(&pair.key);
                    if kind == EvmKeyKind::Empty {
                        continue;
                    }
                    grouped.entry(kind).or_default().push(KvPair {
                        key: stripped.to_vec(),
                        value: pair.value.clone(),
                        delete: pair.delete,
                    });
                }
            }
        }
        grouped
    }

    /// Apply pre-grouped pairs to sub-DBs. Uses parallel threads when multiple
    /// sub-DB types are present, serial application for a single type.
    fn apply_grouped(
        &self,
        version: i64,
        grouped: HashMap<EvmKeyKind, Vec<KvPair>>,
        sync: bool,
    ) -> Result<()> {
        if grouped.len() == 1 {
            let (kind, pairs) = grouped.into_iter().next().unwrap();
            return self.apply_to_sub_db(kind, version, pairs, sync);
        }

        let errors: Vec<MptDbError> = std::thread::scope(|s| {
            let handles: Vec<_> = grouped
                .into_iter()
                .map(|(kind, pairs)| {
                    s.spawn(move || self.apply_to_sub_db(kind, version, pairs, sync))
                })
                .collect();

            handles.into_iter().filter_map(|h| h.join().ok().and_then(|r| r.err())).collect()
        });

        if let Some(first) = errors.into_iter().next() {
            return Err(first);
        }
        Ok(())
    }

    /// Apply pairs to a single sub-DB, constructing the NamedChangeSet wrapper.
    fn apply_to_sub_db(
        &self,
        kind: EvmKeyKind,
        version: i64,
        pairs: Vec<KvPair>,
        sync: bool,
    ) -> Result<()> {
        let db = match self.sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(()),
        };
        let sub_store_key = store_type_name(kind);
        let cs = [NamedChangeSet {
            name: sub_store_key.to_string(),
            changeset: Some(ChangeSet { pairs }),
        }];
        if sync {
            db.apply_changeset_sync(version, &cs)
        } else {
            db.apply_changeset_async(version, &cs)
        }
    }
}

impl StateStore for EVMStateStore {
    fn get(&self, _store_key: &str, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (kind, stripped) = match self.route_key(key) {
            Some(v) => v,
            None => return Ok(None),
        };
        let db = match self.sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(None),
        };
        db.get(store_type_name(kind), version, &stripped)
    }

    fn has(&self, _store_key: &str, version: i64, key: &[u8]) -> Result<bool> {
        let (kind, stripped) = match self.route_key(key) {
            Some(v) => v,
            None => return Ok(false),
        };
        let db = match self.sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(false),
        };
        db.has(store_type_name(kind), version, &stripped)
    }

    fn iterator(
        &self,
        _store_key: &str,
        _version: i64,
        _start: &[u8],
        _end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        Err(MptDbError::Other("evm state store: cross-type iteration not supported".into()))
    }

    fn reverse_iterator(
        &self,
        _store_key: &str,
        _version: i64,
        _start: &[u8],
        _end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        Err(MptDbError::Other("evm state store: cross-type reverse iteration not supported".into()))
    }

    fn raw_iterate(
        &self,
        _store_key: &str,
        _f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool,
    ) -> Result<bool> {
        Err(MptDbError::Other("evm state store: RawIterate not supported".into()))
    }

    fn get_latest_version(&self) -> i64 {
        let mut min_version: i64 = -1;
        for db in self.sub_dbs.values() {
            let v = db.get_latest_version();
            if min_version < 0 || v < min_version {
                min_version = v;
            }
        }
        if min_version < 0 {
            0
        } else {
            min_version
        }
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        for db in self.sub_dbs.values() {
            db.set_latest_version(version)?;
        }
        Ok(())
    }

    fn get_earliest_version(&self) -> i64 {
        let mut min_version: i64 = -1;
        for db in self.sub_dbs.values() {
            let v = db.get_earliest_version();
            if min_version < 0 || v < min_version {
                min_version = v;
            }
        }
        if min_version < 0 {
            0
        } else {
            min_version
        }
    }

    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        for db in self.sub_dbs.values() {
            db.set_earliest_version(version, ignore_version)?;
        }
        Ok(())
    }

    fn apply_changeset_sync(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        let grouped = Self::group_by_sub_type(changesets);
        if grouped.is_empty() {
            return Ok(());
        }
        self.apply_grouped(version, grouped, true)
    }

    fn apply_changeset_async(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        let grouped = Self::group_by_sub_type(changesets);
        if grouped.is_empty() {
            return Ok(());
        }
        self.apply_grouped(version, grouped, false)
    }

    fn prune(&self, version: i64) -> Result<()> {
        if self.sub_dbs.is_empty() {
            return Ok(());
        }

        let errors: Vec<MptDbError> = std::thread::scope(|s| {
            let handles: Vec<_> =
                self.sub_dbs.values().map(|db| s.spawn(move || db.prune(version))).collect();

            handles.into_iter().filter_map(|h| h.join().ok().and_then(|r| r.err())).collect()
        });

        if let Some(first) = errors.into_iter().next() {
            return Err(first);
        }
        Ok(())
    }

    fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()> {
        let mut grouped = HashMap::<EvmKeyKind, Vec<KvPair>>::new();
        let mut pending = 0;

        for node in nodes.iter() {
            let (kind, stripped) = parse_evm_key(&node.key);
            if kind == EvmKeyKind::Empty {
                continue;
            }
            grouped.entry(kind).or_default().push(KvPair {
                key: stripped.to_vec(),
                value: node.value,
                delete: false,
            });
            pending += 1;

            if pending >= IMPORT_BUFFER_SIZE {
                self.apply_grouped(version, std::mem::take(&mut grouped), true)?;
                pending = 0;
            }
        }

        // Flush remaining
        if !grouped.is_empty() {
            self.apply_grouped(version, grouped, true)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let mut last_err: Option<MptDbError> = None;
        for db in self.sub_dbs.values_mut() {
            if let Err(e) = db.close() {
                last_err = Some(e);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::evm_keys::{
        CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX,
    };
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    fn make_nonce_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_codehash_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![CODE_HASH_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_code_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![CODE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_storage_key(addr: &[u8; 20], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = vec![STATE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    fn make_legacy_key(data: &[u8]) -> Vec<u8> {
        // Use a prefix byte that doesn't match any known EVM prefix
        let mut key = vec![0x01];
        key.extend_from_slice(data);
        key
    }

    fn test_addr() -> [u8; 20] {
        let mut addr = [0u8; 20];
        for (i, b) in addr.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        addr
    }

    fn test_slot() -> [u8; 32] {
        let mut slot = [0u8; 32];
        for (i, b) in slot.iter_mut().enumerate() {
            *b = (0xa0 + i) as u8;
        }
        slot
    }

    fn make_evm_changeset(pairs: Vec<(Vec<u8>, Option<&[u8]>)>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet {
            name: EVM_STORE_KEY.to_string(),
            changeset: Some(ChangeSet {
                pairs: pairs
                    .into_iter()
                    .map(|(k, v)| KvPair {
                        delete: v.is_none(),
                        key: k,
                        value: v.unwrap_or_default().to_vec(),
                    })
                    .collect(),
            }),
        }]
    }

    fn open_evm_store(dir: &std::path::Path) -> EVMStateStore {
        let cfg = test_config(dir);
        EVMStateStore::new(&dir.to_string_lossy(), &cfg).unwrap()
    }

    #[test]
    fn test_evm_store_get_has() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"42"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        let val = store.get("evm", 1, &nonce_key).unwrap();
        assert_eq!(val, Some(b"42".to_vec()));

        assert!(store.has("evm", 1, &nonce_key).unwrap());

        // Non-existent key
        let addr2 = [0xffu8; 20];
        let missing_key = make_nonce_key(&addr2);
        assert_eq!(store.get("evm", 1, &missing_key).unwrap(), None);
        assert!(!store.has("evm", 1, &missing_key).unwrap());
    }

    #[test]
    fn test_evm_store_all_sub_dbs() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let slot = test_slot();

        let nonce_key = make_nonce_key(&addr);
        let codehash_key = make_codehash_key(&addr);
        let code_key = make_code_key(&addr);
        let storage_key = make_storage_key(&addr, &slot);
        let legacy_key = make_legacy_key(b"some_mapping");

        let cs = make_evm_changeset(vec![
            (nonce_key.clone(), Some(b"1")),
            (codehash_key.clone(), Some(b"hash_val")),
            (code_key.clone(), Some(b"bytecode")),
            (storage_key.clone(), Some(b"slot_val")),
            (legacy_key.clone(), Some(b"legacy_val")),
        ]);
        store.apply_changeset_sync(1, &cs).unwrap();

        assert_eq!(store.get("evm", 1, &nonce_key).unwrap(), Some(b"1".to_vec()));
        assert_eq!(store.get("evm", 1, &codehash_key).unwrap(), Some(b"hash_val".to_vec()));
        assert_eq!(store.get("evm", 1, &code_key).unwrap(), Some(b"bytecode".to_vec()));
        assert_eq!(store.get("evm", 1, &storage_key).unwrap(), Some(b"slot_val".to_vec()));
        assert_eq!(store.get("evm", 1, &legacy_key).unwrap(), Some(b"legacy_val".to_vec()));
    }

    #[test]
    fn test_evm_store_version_tracking() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());

        assert_eq!(store.get_latest_version(), 0);
        assert_eq!(store.get_earliest_version(), 0);

        store.set_latest_version(10).unwrap();
        assert_eq!(store.get_latest_version(), 10);

        store.set_earliest_version(5, false).unwrap();
        assert_eq!(store.get_earliest_version(), 5);
    }

    #[test]
    fn test_evm_store_delete_tombstone() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"42"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        assert!(store.has("evm", 1, &nonce_key).unwrap());

        // Delete (tombstone)
        let cs = make_evm_changeset(vec![(nonce_key.clone(), None)]);
        store.apply_changeset_sync(2, &cs).unwrap();
        assert!(!store.has("evm", 2, &nonce_key).unwrap());
        assert_eq!(store.get("evm", 2, &nonce_key).unwrap(), None);
    }

    #[test]
    fn test_evm_store_prune() {
        // EVM sub-DBs use use_default_comparer=true (matching Go's PebbleDB).
        // The RocksDB MVCC prune seek optimisation requires the custom
        // comparator, so we open with use_default_comparer=false here to
        // exercise the prune-delegation logic without hitting the seek issue.
        let dir = tempdir().unwrap();
        let cfg = StateStoreConfig {
            db_directory: dir.path().to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        };
        // Open sub-DBs with the MVCC comparator for correct prune behaviour.
        let mut sub_dbs =
            HashMap::<EvmKeyKind, Box<dyn StateStore>>::with_capacity(all_evm_store_types().len());
        for store_type in all_evm_store_types() {
            let sub_dir = std::path::Path::new(&cfg.db_directory).join(store_type_name(store_type));
            let mut sub_cfg = cfg.clone();
            sub_cfg.db_directory = sub_dir.to_string_lossy().to_string();
            sub_cfg.use_default_comparer = false;
            let db = MvccDatabase::open_db(&sub_cfg).unwrap();
            sub_dbs.insert(store_type, Box::new(db));
        }
        let store = EVMStateStore { sub_dbs };

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write version 1 and 2
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"v1"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"v2"))]);
        store.apply_changeset_sync(2, &cs).unwrap();

        // Prune version 1
        store.prune(1).unwrap();

        // Version 2 should still be available
        assert_eq!(store.get("evm", 2, &nonce_key).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_evm_store_non_evm_ignored() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Changeset with non-EVM store name should be ignored
        let cs = vec![NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: nonce_key.clone(),
                    value: b"100".to_vec(),
                }],
            }),
        }];
        store.apply_changeset_sync(1, &cs).unwrap();

        // Key should not exist
        assert_eq!(store.get("evm", 1, &nonce_key).unwrap(), None);
    }

    #[test]
    fn test_evm_store_iterator_not_supported() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());

        assert!(store.iterator("evm", 1, b"a", b"z").is_err());
        assert!(store.reverse_iterator("evm", 1, b"a", b"z").is_err());
        assert!(store.raw_iterate("evm", &mut |_, _, _| true).is_err());
    }

    #[test]
    fn test_evm_store_parallel_apply() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let slot = test_slot();

        // Build a changeset touching multiple sub-DB types
        let nonce_key = make_nonce_key(&addr);
        let storage_key = make_storage_key(&addr, &slot);
        let code_key = make_code_key(&addr);

        let cs = make_evm_changeset(vec![
            (nonce_key.clone(), Some(b"10")),
            (storage_key.clone(), Some(b"slot_data")),
            (code_key.clone(), Some(b"0xdeadbeef")),
        ]);
        store.apply_changeset_sync(1, &cs).unwrap();

        assert_eq!(store.get("evm", 1, &nonce_key).unwrap(), Some(b"10".to_vec()));
        assert_eq!(store.get("evm", 1, &storage_key).unwrap(), Some(b"slot_data".to_vec()));
        assert_eq!(store.get("evm", 1, &code_key).unwrap(), Some(b"0xdeadbeef".to_vec()));
    }

    #[test]
    fn test_evm_store_close() {
        let dir = tempdir().unwrap();
        let mut store = open_evm_store(dir.path());
        store.close().unwrap();
    }
}
