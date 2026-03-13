use crossbeam_channel::Receiver;
use seidb_common::error::Result;
use seidb_proto::NamedChangeSet;
use seidb_traits::{iterator::DbIterator, ss::StateStore, types::SnapshotNode};
use std::sync::Arc;

/// Thin wrapper that delegates all [`StateStore`] operations to an inner implementation.
///
/// This is the SS-layer adapter for the main Cosmos state (all non-EVM modules).
pub struct CosmosStateStore {
    db: Arc<dyn StateStore>,
}

impl CosmosStateStore {
    pub fn new(db: Arc<dyn StateStore>) -> Self {
        Self { db }
    }
}

impl StateStore for CosmosStateStore {
    fn get(&self, store_key: &str, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db.get(store_key, version, key)
    }

    fn has(&self, store_key: &str, version: i64, key: &[u8]) -> Result<bool> {
        self.db.has(store_key, version, key)
    }

    fn iterator(
        &self,
        store_key: &str,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        self.db.iterator(store_key, version, start, end)
    }

    fn reverse_iterator(
        &self,
        store_key: &str,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        self.db.reverse_iterator(store_key, version, start, end)
    }

    fn raw_iterate(
        &self,
        store_key: &str,
        f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool,
    ) -> Result<bool> {
        self.db.raw_iterate(store_key, f)
    }

    fn get_latest_version(&self) -> i64 {
        self.db.get_latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.db.set_latest_version(version)
    }

    fn get_earliest_version(&self) -> i64 {
        self.db.get_earliest_version()
    }

    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        self.db.set_earliest_version(version, ignore_version)
    }

    fn apply_changeset_sync(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        self.db.apply_changeset_sync(version, changesets)
    }

    fn apply_changeset_async(&self, version: i64, changesets: &[NamedChangeSet]) -> Result<()> {
        self.db.apply_changeset_async(version, changesets)
    }

    fn prune(&self, version: i64) -> Result<()> {
        self.db.prune(version)
    }

    fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()> {
        self.db.import(version, nodes)
    }

    fn close(&mut self) -> Result<()> {
        // Try to get exclusive access for close. If other refs exist
        // (e.g. async writer thread), close will happen on final Arc drop.
        if let Some(db) = Arc::get_mut(&mut self.db) {
            db.close()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::config::StateStoreConfig;
    use seidb_engine::mvcc::db::MvccDatabase;
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
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

    fn open_cosmos(dir: &std::path::Path) -> CosmosStateStore {
        let cfg = test_config(dir);
        let db = MvccDatabase::open_db(&cfg).unwrap();
        CosmosStateStore::new(Arc::new(db))
    }

    #[test]
    fn test_cosmos_store_get_set() {
        let dir = tempdir().unwrap();
        let store = open_cosmos(dir.path());

        let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        let val = store.get("bank", 1, b"alice").unwrap();
        assert_eq!(val, Some(b"100".to_vec()));

        // Non-existent key returns None.
        let val = store.get("bank", 1, b"bob").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_cosmos_store_has() {
        let dir = tempdir().unwrap();
        let store = open_cosmos(dir.path());

        let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        assert!(store.has("bank", 1, b"alice").unwrap());
        assert!(!store.has("bank", 1, b"missing").unwrap());
    }

    #[test]
    fn test_cosmos_store_delete() {
        let dir = tempdir().unwrap();
        let store = open_cosmos(dir.path());

        // Set a key.
        let cs = make_changeset("bank", vec![(b"alice", Some(b"100"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        assert_eq!(store.get("bank", 1, b"alice").unwrap(), Some(b"100".to_vec()));

        // Delete the key (None value = tombstone).
        let cs = make_changeset("bank", vec![(b"alice", None)]);
        store.apply_changeset_sync(2, &cs).unwrap();
        assert_eq!(store.get("bank", 2, b"alice").unwrap(), None);
    }

    #[test]
    fn test_cosmos_store_versions() {
        let dir = tempdir().unwrap();
        let store = open_cosmos(dir.path());

        assert_eq!(store.get_latest_version(), 0);
        store.set_latest_version(42).unwrap();
        assert_eq!(store.get_latest_version(), 42);

        store.set_earliest_version(10, false).unwrap();
        assert_eq!(store.get_earliest_version(), 10);
    }

    #[test]
    fn test_cosmos_store_iterator() {
        let dir = tempdir().unwrap();
        let store = open_cosmos(dir.path());

        let cs = make_changeset(
            "bank",
            vec![(b"a", Some(b"1")), (b"b", Some(b"2")), (b"c", Some(b"3"))],
        );
        store.apply_changeset_sync(1, &cs).unwrap();

        let mut iter = store.iterator("bank", 1, b"a", b"d").unwrap();
        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(iter.key().to_vec());
            iter.next();
        }
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn test_cosmos_store_close_idempotent() {
        let dir = tempdir().unwrap();
        let mut store = open_cosmos(dir.path());
        store.close().unwrap();
        store.close().unwrap(); // second close must not panic
    }
}
