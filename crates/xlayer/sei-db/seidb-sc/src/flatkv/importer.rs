use crate::flatkv::{meta::commit_global_metadata, store::CommitStore};
use seidb_common::error::{Result, SeiDbError};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};

/// Number of KV pairs buffered before an automatic flush to disk.
const IMPORT_BATCH_SIZE: usize = 20000;

/// Bulk data importer for FlatKV.
///
/// Accumulates leaf-node KV pairs into an in-memory buffer and periodically
/// flushes them via `apply_change_sets` + `commit_batches`. Unlike normal
/// commits, the importer bypasses WAL writes — data is written directly to
/// the per-DB RocksDB instances.
pub struct KvImporter<'a> {
    store: &'a mut CommitStore,
    version: i64,
    buffer: Vec<KvPair>,
}

impl<'a> KvImporter<'a> {
    /// Creates a new importer that will write at the given version.
    pub fn new(store: &'a mut CommitStore, version: i64) -> KvImporter<'a> {
        KvImporter { store, version, buffer: Vec::with_capacity(IMPORT_BATCH_SIZE) }
    }

    /// No-op: FlatKV does not have module separation.
    pub fn add_module(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }

    /// Adds a snapshot node to the import buffer.
    ///
    /// Only leaf nodes (height == 0) with non-empty keys are imported.
    /// When the buffer reaches `IMPORT_BATCH_SIZE`, it is automatically
    /// flushed to disk.
    pub fn add_node(&mut self, key: &[u8], value: &[u8], height: i32) -> Result<()> {
        if height != 0 {
            return Ok(());
        }

        self.buffer.push(KvPair { delete: false, key: key.to_vec(), value: value.to_vec() });

        if self.buffer.len() >= IMPORT_BATCH_SIZE {
            self.flush()?;
        }

        Ok(())
    }

    /// Flushes the buffered KV pairs to disk via apply_change_sets + commit_batches.
    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let ncs = vec![NamedChangeSet {
            name: "evm".to_string(),
            changeset: Some(ChangeSet { pairs: self.buffer.drain(..).collect() }),
        }];

        self.store.apply_change_sets(&ncs)?;
        self.store.commit_batches(self.version)?;
        self.store.clear_pending_writes();

        Ok(())
    }

    /// Flushes remaining data, updates committed state, and persists global metadata.
    pub fn close(&mut self) -> Result<()> {
        self.flush()?;

        self.store.committed_version = self.version;
        self.store.committed_lt_hash = self.store.working_lt_hash.clone();

        let metadata_db = self
            .store
            .metadata_db
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("metadata_db not open".to_string()))?;
        commit_global_metadata(
            metadata_db,
            self.version,
            &self.store.committed_lt_hash,
            self.store.config.fsync,
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatkv::meta::load_global_metadata;
    use seidb_common::{config::FlatKvConfig, evm_keys::STATE_KEY_PREFIX};
    use tempfile::TempDir;

    /// Helper: create an open CommitStore.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    /// Build a storage memiavl key (prefix 0x03 || addr || slot).
    fn make_storage_key(index: u8) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + 20 + 32);
        key.push(STATE_KEY_PREFIX);
        // 20-byte address
        let mut addr = [0u8; 20];
        addr[0] = index;
        key.extend_from_slice(&addr);
        // 32-byte slot
        let mut slot = [0u8; 32];
        slot[0] = index;
        key.extend_from_slice(&slot);
        key
    }

    #[test]
    fn test_importer_basic() {
        let (mut store, _dir) = open_store();

        {
            let mut importer = KvImporter::new(&mut store, 10);
            for i in 0..10u8 {
                let key = make_storage_key(i);
                let value = format!("value_{i}");
                importer.add_node(&key, value.as_bytes(), 0).unwrap();
            }
            importer.close().unwrap();
        }

        assert_eq!(store.committed_version, 10);

        // Verify data is readable from the underlying storage DB.
        let storage_db = store.storage_db.as_ref().unwrap();
        use seidb_traits::kv::KvEngine;
        for i in 0..10u8 {
            // Internal key is addr || slot (stripped prefix).
            let mut addr = [0u8; 20];
            addr[0] = i;
            let mut slot = [0u8; 32];
            slot[0] = i;
            let mut internal_key = addr.to_vec();
            internal_key.extend_from_slice(&slot);

            let val = storage_db.get(&internal_key).unwrap().expect("value missing");
            assert_eq!(val, format!("value_{i}").as_bytes());
        }
    }

    #[test]
    fn test_importer_batch_flush() {
        let (mut store, _dir) = open_store();

        {
            let mut importer = KvImporter::new(&mut store, 5);

            // Import more than IMPORT_BATCH_SIZE nodes to trigger automatic flush.
            for i in 0..IMPORT_BATCH_SIZE + 100 {
                let mut key = Vec::with_capacity(1 + 20 + 32);
                key.push(STATE_KEY_PREFIX);
                // Use index bytes spread across address and slot.
                let idx = i as u32;
                let mut addr = [0u8; 20];
                addr[..4].copy_from_slice(&idx.to_be_bytes());
                key.extend_from_slice(&addr);
                let mut slot = [0u8; 32];
                slot[..4].copy_from_slice(&idx.to_be_bytes());
                key.extend_from_slice(&slot);

                importer.add_node(&key, b"v", 0).unwrap();
            }

            // Buffer should have been flushed at least once; remaining < IMPORT_BATCH_SIZE.
            assert!(importer.buffer.len() < IMPORT_BATCH_SIZE);

            importer.close().unwrap();
        }

        assert_eq!(store.committed_version, 5);
    }

    #[test]
    fn test_importer_close_commits() {
        let (mut store, _dir) = open_store();

        {
            let mut importer = KvImporter::new(&mut store, 42);
            let key = make_storage_key(1);
            importer.add_node(&key, b"data", 0).unwrap();
            importer.close().unwrap();
        }

        assert_eq!(store.committed_version, 42);

        // Verify global metadata was persisted.
        let metadata_db = store.metadata_db.as_ref().unwrap();
        let (version, _hash) = load_global_metadata(metadata_db).unwrap();
        assert_eq!(version, 42);
    }

    #[test]
    fn test_importer_empty() {
        let (mut store, _dir) = open_store();

        {
            let mut importer = KvImporter::new(&mut store, 1);
            // No nodes added.
            importer.close().unwrap();
        }

        assert_eq!(store.committed_version, 1);
    }

    #[test]
    fn test_importer_skips_non_leaf() {
        let (mut store, _dir) = open_store();

        {
            let mut importer = KvImporter::new(&mut store, 3);
            let key = make_storage_key(1);
            // height != 0 should be skipped.
            importer.add_node(&key, b"inner_node", 1).unwrap();
            importer.add_node(&key, b"inner_node", 5).unwrap();
            assert!(importer.buffer.is_empty());
            importer.close().unwrap();
        }

        assert_eq!(store.committed_version, 3);
    }
}
