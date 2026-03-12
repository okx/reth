use crate::flatkv::{store::CommitStore, FlatKvStore};
use seidb_common::{
    error::{Result, SeiDbError},
    evm_keys::EvmKeyKind,
};
use seidb_proto::NamedChangeSet;
use seidb_traits::{
    iterator::DbIterator,
    sc::{Exporter, Importer},
};

impl FlatKvStore for CommitStore {
    fn load_version(&mut self, target_version: i64) -> Result<()> {
        self.load_version(target_version)
    }

    fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()> {
        self.apply_change_sets(cs)
    }

    fn commit(&mut self) -> Result<i64> {
        self.commit()
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get(key).0
    }

    fn has(&self, key: &[u8]) -> bool {
        self.has(key)
    }

    fn iterator(&self, start: &[u8], end: &[u8]) -> Box<dyn DbIterator> {
        // Return memiavl-format keys (with 0x03 storage prefix) so that
        // CompositeCommitStore and Exporter see a uniform keyspace matching MemIAVL.
        self.iterator_memiavl(start, end, EvmKeyKind::Storage)
    }

    fn iterator_by_prefix(&self, prefix: &[u8]) -> Box<dyn DbIterator> {
        self.iterator_by_prefix(prefix)
    }

    fn root_hash(&self) -> Vec<u8> {
        self.root_hash()
    }

    fn version(&self) -> i64 {
        self.version()
    }

    fn write_snapshot(&mut self) -> Result<()> {
        self.write_snapshot()
    }

    fn rollback(&mut self, target_version: i64) -> Result<()> {
        self.rollback(target_version)
    }

    fn exporter(&self, _version: i64) -> Result<Box<dyn Exporter>> {
        Err(SeiDbError::Other("exporter not implemented for FlatKV".into()))
    }

    fn importer(&self, _version: i64) -> Result<Box<dyn Importer>> {
        Err(SeiDbError::Other("importer via trait not implemented; use KvImporter directly".into()))
    }

    fn close(&mut self) -> Result<()> {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::{config::FlatKvConfig, evm_keys::STATE_KEY_PREFIX};
    use seidb_traits::{kv::KvEngine, types::WriteOptions};
    use tempfile::TempDir;

    #[test]
    fn test_commit_store_implements_flatkv_store() {
        // Compile-time assertion that CommitStore implements FlatKvStore.
        fn _assert(_: Box<dyn FlatKvStore>) {}
    }

    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    #[test]
    fn test_trait_impl_iterator_returns_memiavl_keys() {
        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();

        // Write internal-format keys (no prefix) directly to storage_db.
        let addr_slot = [0x11u8; 52]; // 20-byte addr + 32-byte slot
        db.set(&addr_slot, b"storage_value", &wo).unwrap();

        // Use FlatKvStore trait method — should return memiavl-format keys.
        let trait_store: &dyn FlatKvStore = &store;
        let mut iter = trait_store.iterator(b"", b"");

        assert!(iter.valid());
        let key = iter.key();
        assert_eq!(key.len(), 1 + 52, "trait iterator key should have memiavl prefix");
        assert_eq!(key[0], STATE_KEY_PREFIX, "should be prefixed with 0x03");
        assert_eq!(&key[1..], &addr_slot);
        assert_eq!(iter.value(), b"storage_value");

        iter.next();
        assert!(!iter.valid());
    }
}
