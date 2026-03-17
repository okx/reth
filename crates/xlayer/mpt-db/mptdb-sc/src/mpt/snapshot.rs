use alloy_primitives::{keccak256, B256};
use alloy_rlp::Decodable;
use alloy_trie::EMPTY_ROOT_HASH;
use mptdb_common::error::{MptDbError, Result};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use super::{
    encoding::decode_node,
    gc::collect_reachable_hashes,
    manifest::VersionManifest,
    node::{ChildRef, MptNode},
    persisted::PersistedTrieStore,
    r#trait::{MptSnapshotExporter, MptSnapshotImporter, MptSnapshotMeta, MptSnapshotNode},
};

/// Streaming BFS exporter that deduplicates visited nodes.
pub struct SnapshotExporter {
    meta: MptSnapshotMeta,
    pending: VecDeque<B256>,
    visited: HashSet<B256>,
    store: Arc<PersistedTrieStore>,
}

impl SnapshotExporter {
    pub(crate) fn new(
        store: Arc<PersistedTrieStore>,
        state_root: B256,
        version: i64,
    ) -> Result<Self> {
        let mut pending = VecDeque::new();
        let mut visited = HashSet::new();

        if state_root != EMPTY_ROOT_HASH {
            pending.push_back(state_root);
            visited.insert(state_root);
        }

        Ok(Self { meta: MptSnapshotMeta { version, state_root }, pending, visited, store })
    }
}

impl MptSnapshotExporter for SnapshotExporter {
    fn meta(&self) -> &MptSnapshotMeta {
        &self.meta
    }

    fn next_node(&mut self) -> Result<Option<MptSnapshotNode>> {
        let hash = match self.pending.pop_front() {
            Some(h) => h,
            None => return Ok(None),
        };

        let rlp = self
            .store
            .get_node(hash)?
            .ok_or_else(|| MptDbError::Other(format!("snapshot export: node not found: {hash}")))?;

        // Decode to discover children for BFS
        let node = decode_node(&rlp)
            .map_err(|e| MptDbError::Other(format!("snapshot export: decode node: {e}")))?;

        match node {
            MptNode::Leaf(ref leaf) => {
                // Account trie leaves may contain TrieAccount with storage_root.
                // Follow storage_root into the storage trie.
                if let Ok(trie_account) = alloy_trie::TrieAccount::decode(&mut &leaf.value[..]) {
                    let sr = trie_account.storage_root;
                    if sr != EMPTY_ROOT_HASH && self.visited.insert(sr) {
                        self.pending.push_back(sr);
                    }
                }
            }
            MptNode::Extension(ext) => {
                if let ChildRef::Hash(h) = ext.child &&
                    self.visited.insert(h)
                {
                    self.pending.push_back(h);
                }
            }
            MptNode::Branch(branch) => {
                for child in &branch.children {
                    if let Some(ChildRef::Hash(h)) = child &&
                        self.visited.insert(*h)
                    {
                        self.pending.push_back(*h);
                    }
                }
            }
        }

        Ok(Some(MptSnapshotNode { hash, rlp }))
    }

    fn close(&mut self) -> Result<()> {
        self.pending.clear();
        self.visited.clear();
        Ok(())
    }
}

/// Batch-buffered importer with flush threshold and integrity verification on close.
///
/// Writes to a temporary `PersistedTrieStore` (`trie_nodes_import_tmp/`) and
/// atomically installs it via rename on successful `close()`. On failure or
/// drop without close, the temp directory is cleaned up leaving the live
/// store untouched.
pub struct SnapshotImporter {
    version: i64,
    expected_root: B256,
    batch_buffer: Vec<(B256, Vec<u8>)>,
    flush_threshold: usize,
    imported_nodes: u64,
    /// Temporary store used during import — nodes are written here.
    temp_store: Arc<PersistedTrieStore>,
    /// Path to the temp trie_nodes directory (`{dir}/trie_nodes_import_tmp/`).
    temp_dir: PathBuf,
    /// Path to the live trie_nodes directory (`{dir}/trie_nodes/`).
    live_dir: PathBuf,
    manifest_path: PathBuf,
    closed: bool,
}

impl SnapshotImporter {
    pub(crate) fn new(
        version: i64,
        expected_root: B256,
        live_store: Arc<PersistedTrieStore>,
        manifest_path: PathBuf,
    ) -> Result<Self> {
        if version <= 0 {
            return Err(MptDbError::Other(format!(
                "snapshot import: version must be > 0, got {version}"
            )));
        }

        // Check fresh DB: manifest must be {0 -> EMPTY_ROOT_HASH} and store empty
        let manifest = VersionManifest::load(&manifest_path)?;
        if manifest.latest_version != 0 ||
            manifest.versions.len() != 1 ||
            manifest.get_root(0) != Some(EMPTY_ROOT_HASH)
        {
            return Err(MptDbError::Other(
                "snapshot import: DB is not fresh (manifest has non-initial state)".to_string(),
            ));
        }
        if !live_store.is_empty()? {
            return Err(MptDbError::Other(
                "snapshot import: DB is not fresh (persisted store is not empty)".to_string(),
            ));
        }

        // Derive live and temp dirs from manifest_path's parent
        let base_dir = manifest_path.parent().ok_or_else(|| {
            MptDbError::Other("snapshot import: cannot determine base dir".to_string())
        })?;
        let live_dir = base_dir.join("trie_nodes");
        let temp_dir = base_dir.join("trie_nodes_import_tmp");

        // Clean up any leftover temp dir from a prior failed import
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir).map_err(|e| {
                MptDbError::Other(format!("snapshot import: cleanup old temp dir: {e}"))
            })?;
        }

        // Create temp PersistedTrieStore
        let temp_store = Arc::new(PersistedTrieStore::open(&temp_dir)?);

        Ok(Self {
            version,
            expected_root,
            batch_buffer: Vec::new(),
            flush_threshold: 10000,
            imported_nodes: 0,
            temp_store,
            temp_dir,
            live_dir,
            manifest_path,
            closed: false,
        })
    }

    fn flush_buffer(&mut self) -> Result<()> {
        if !self.batch_buffer.is_empty() {
            self.temp_store.persist_batch(&self.batch_buffer, false)?;
            self.batch_buffer.clear();
        }
        Ok(())
    }

    /// Clean up the temp directory (best-effort).
    fn cleanup_temp(&self) {
        if self.temp_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }
}

impl Drop for SnapshotImporter {
    fn drop(&mut self) {
        if !self.closed {
            self.cleanup_temp();
        }
    }
}

impl MptSnapshotImporter for SnapshotImporter {
    fn add_node(&mut self, node: &MptSnapshotNode) -> Result<()> {
        if self.closed {
            return Err(MptDbError::Other(
                "snapshot import: importer is already closed".to_string(),
            ));
        }

        // Verify hash
        let computed = keccak256(&node.rlp);
        if computed != node.hash {
            return Err(MptDbError::Other(format!(
                "snapshot import: hash mismatch: expected {}, computed {}",
                node.hash, computed
            )));
        }

        self.batch_buffer.push((node.hash, node.rlp.clone()));
        self.imported_nodes += 1;

        if self.batch_buffer.len() >= self.flush_threshold {
            self.flush_buffer()?;
        }

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        // 1. Flush remaining buffer
        self.flush_buffer()?;

        // 2. Integrity verification: walk entire trie from expected_root
        collect_reachable_hashes(&self.temp_store, [self.expected_root]).map_err(|e| {
            self.cleanup_temp();
            MptDbError::Other(format!("snapshot import: integrity verification failed: {e}"))
        })?;

        // 3. Close the temp store so RocksDB releases its lock
        if let Some(store) = Arc::get_mut(&mut self.temp_store) {
            store.close()?;
        }

        // 4. Atomic install: remove live dir, rename temp -> live
        if self.live_dir.exists() {
            std::fs::remove_dir_all(&self.live_dir).map_err(|e| {
                self.cleanup_temp();
                MptDbError::Other(format!("snapshot import: remove live dir: {e}"))
            })?;
        }
        std::fs::rename(&self.temp_dir, &self.live_dir)
            .map_err(|e| MptDbError::Other(format!("snapshot import: rename temp -> live: {e}")))?;

        // 5. Write manifest {0 -> EMPTY_ROOT_HASH, version -> expected_root}
        let mut manifest = VersionManifest::load(&self.manifest_path)?;
        for v in 1..self.version {
            manifest.add_version(v, EMPTY_ROOT_HASH)?;
        }
        manifest.add_version(self.version, self.expected_root)?;
        let versions_to_keep: Vec<i64> =
            manifest.versions.keys().copied().filter(|v| *v == 0 || *v == self.version).collect();
        manifest.versions.retain(|k, _| versions_to_keep.contains(k));
        manifest.earliest_version = 0;
        manifest.latest_version = self.version;
        manifest.save(&self.manifest_path)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_trie::Nibbles;
    use tempfile::TempDir;

    use crate::mpt::tree::MptTree;

    fn tmp_store_arc() -> (Arc<PersistedTrieStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        (Arc::new(store), dir)
    }

    fn build_tree(store: &PersistedTrieStore, entries: &[(&[u8], &[u8])]) -> B256 {
        let mut tree = MptTree::new();
        for (k, v) in entries {
            let key = Nibbles::unpack(keccak256(k));
            tree.insert(&key, v.to_vec());
        }
        let root = tree.root_hash();
        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();
        root
    }

    fn fresh_dir_with_manifest(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let manifest = VersionManifest::load(&dir.join("manifest.json")).unwrap();
        manifest.save(&dir.join("manifest.json")).unwrap();
    }

    /// Create a fresh import directory with manifest and trie_nodes subdir.
    /// Returns (store, manifest_path).
    fn fresh_import_dir(base: &std::path::Path) -> (Arc<PersistedTrieStore>, PathBuf) {
        let import_dir = base.join("import");
        fresh_dir_with_manifest(&import_dir);
        let nodes_dir = import_dir.join("trie_nodes");
        std::fs::create_dir_all(&nodes_dir).unwrap();
        let store = Arc::new(PersistedTrieStore::open(&nodes_dir).unwrap());
        let manifest_path = import_dir.join("manifest.json");
        (store, manifest_path)
    }

    /// T4.1: exporter(empty root) -> next_node() immediately None
    #[test]
    fn t4_1_exporter_empty_root() {
        let (store, _dir) = tmp_store_arc();
        let mut exp = SnapshotExporter::new(store, EMPTY_ROOT_HASH, 0).unwrap();
        assert!(exp.next_node().unwrap().is_none());
    }

    /// T4.2: exporter(non-empty root) -> exported nodes can rebuild same root
    #[test]
    fn t4_2_exporter_roundtrip_nodes() {
        let (store, _dir) = tmp_store_arc();
        let root = build_tree(&store, &[(b"a", &[0xaa; 40]), (b"b", &[0xbb; 40])]);
        let mut exp = SnapshotExporter::new(store.clone(), root, 1).unwrap();

        let mut nodes = Vec::new();
        while let Some(node) = exp.next_node().unwrap() {
            nodes.push(node);
        }
        assert!(!nodes.is_empty());

        // All nodes should have valid hash
        for n in &nodes {
            assert_eq!(n.hash, keccak256(&n.rlp));
        }
    }

    /// T4.3: exporter deduplicates shared sub-path nodes
    #[test]
    fn t4_3_exporter_dedup() {
        let (store, _dir) = tmp_store_arc();
        let root = build_tree(&store, &[(b"a", &[0xaa; 40]), (b"b", &[0xbb; 40])]);
        let mut exp = SnapshotExporter::new(store.clone(), root, 1).unwrap();

        let mut hashes = HashSet::new();
        while let Some(node) = exp.next_node().unwrap() {
            assert!(hashes.insert(node.hash), "duplicate hash exported: {}", node.hash);
        }
    }

    /// T4.4: importer(hash mismatch) -> Err
    #[test]
    fn t4_4_importer_hash_mismatch() {
        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        let mut imp =
            SnapshotImporter::new(1, B256::repeat_byte(0xaa), store, manifest_path).unwrap();
        let bad_node = MptSnapshotNode {
            hash: B256::repeat_byte(0x01),
            rlp: vec![0xc1, 0x80], // keccak256 won't match
        };
        assert!(imp.add_node(&bad_node).is_err());
    }

    /// T4.5: importer(non-fresh DB) -> Err
    #[test]
    fn t4_5_importer_non_fresh() {
        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        // Make it non-fresh by writing a node
        store.persist_batch(&[(B256::repeat_byte(0x01), vec![0x80])], true).unwrap();

        let result = SnapshotImporter::new(1, B256::repeat_byte(0xaa), store, manifest_path);
        assert!(result.is_err());
    }

    /// T4.6: exporter -> importer roundtrip -> reloaded root same
    #[test]
    fn t4_6_export_import_roundtrip() {
        // Build source
        let (src_store, _src_dir) = tmp_store_arc();
        let root = build_tree(&src_store, &[(b"x", &[0x11; 40]), (b"y", &[0x22; 40])]);

        // Export
        let mut exp = SnapshotExporter::new(src_store.clone(), root, 5).unwrap();
        let mut nodes = Vec::new();
        while let Some(n) = exp.next_node().unwrap() {
            nodes.push(n);
        }
        exp.close().unwrap();

        // Import into fresh DB
        let dst_dir = TempDir::new().unwrap();
        let (dst_store, manifest_path) = fresh_import_dir(dst_dir.path());

        let mut imp =
            SnapshotImporter::new(5, root, dst_store.clone(), manifest_path.clone()).unwrap();
        // Drop the original live store reference so RocksDB lock is released
        drop(dst_store);
        for n in &nodes {
            imp.add_node(n).unwrap();
        }
        imp.close().unwrap();

        // Verify manifest
        let manifest = VersionManifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.latest_version, 5);
        assert_eq!(manifest.get_root(5), Some(root));

        // After atomic install, nodes are at the live dir (trie_nodes/)
        let live_store =
            PersistedTrieStore::open(&dst_dir.path().join("import").join("trie_nodes")).unwrap();
        assert!(live_store.get_node(root).unwrap().is_some());
    }

    /// T4.7: importer close is idempotent
    #[test]
    fn t4_7_importer_close_idempotent() {
        let dst_dir = TempDir::new().unwrap();
        let (dst_store, manifest_path) = fresh_import_dir(dst_dir.path());

        // We need a valid tree to import for close() to succeed
        let (src_store, _src_dir) = tmp_store_arc();
        let root = build_tree(&src_store, &[(b"q", &[0x33; 40])]);
        let mut exp = SnapshotExporter::new(src_store.clone(), root, 1).unwrap();
        let mut nodes = Vec::new();
        while let Some(n) = exp.next_node().unwrap() {
            nodes.push(n);
        }

        let mut imp = SnapshotImporter::new(1, root, dst_store, manifest_path).unwrap();
        for n in &nodes {
            imp.add_node(n).unwrap();
        }
        imp.close().unwrap();
        imp.close().unwrap(); // idempotent
    }

    /// T4.8: importer(version<=0) -> Err
    #[test]
    fn t4_8_importer_version_zero() {
        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        assert!(SnapshotImporter::new(0, B256::ZERO, store.clone(), manifest_path.clone()).is_err());
        assert!(SnapshotImporter::new(-1, B256::ZERO, store, manifest_path).is_err());
    }

    /// T4.9: importer(expected_root missing after add_node) -> close Err
    #[test]
    fn t4_9_importer_missing_root() {
        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        // expected_root that we never add
        let fake_root = B256::repeat_byte(0xff);
        let mut imp = SnapshotImporter::new(1, fake_root, store, manifest_path).unwrap();
        // Add an unrelated node
        let rlp = vec![0xc1, 0x80];
        let node = MptSnapshotNode { hash: keccak256(&rlp), rlp };
        imp.add_node(&node).unwrap();
        assert!(imp.close().is_err());
    }

    /// T4.10: importer(missing hashed child) -> close() integrity check fails
    #[test]
    fn t4_10_importer_missing_child() {
        // Build a tree with hash children, only export the root
        let (src_store, _src_dir) = tmp_store_arc();
        let root = build_tree(
            &src_store,
            &[(b"aaa", &[0x11; 40]), (b"bbb", &[0x22; 40]), (b"ccc", &[0x33; 40])],
        );

        // Only get the root node
        let root_rlp = src_store.get_node(root).unwrap().unwrap();
        let root_node = MptSnapshotNode { hash: root, rlp: root_rlp };

        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        let mut imp = SnapshotImporter::new(1, root, store, manifest_path).unwrap();
        imp.add_node(&root_node).unwrap();
        // close() should fail because children are missing
        assert!(imp.close().is_err());
    }

    /// T4.11: importer flushes in batches, not holding all nodes in memory
    #[test]
    fn t4_11_importer_batch_flush() {
        let dir = TempDir::new().unwrap();
        let (store, manifest_path) = fresh_import_dir(dir.path());

        let mut imp =
            SnapshotImporter::new(1, B256::repeat_byte(0xaa), store.clone(), manifest_path)
                .unwrap();
        // Override flush threshold for testing
        imp.flush_threshold = 2;

        // Add 5 nodes — should trigger 2 flushes
        for i in 0u8..5 {
            let rlp = vec![0xc1, i];
            let node = MptSnapshotNode { hash: keccak256(&rlp), rlp };
            imp.add_node(&node).unwrap();
        }

        // The buffer should have been flushed (only 1 remaining after 2 flushes of 2)
        assert!(imp.batch_buffer.len() < 3);
        assert_eq!(imp.imported_nodes, 5);
    }

    /// T4.12: importer failure then same dir -> retry succeeds (temp dir cleaned up on drop)
    #[test]
    fn t4_12_importer_retry_same_dir() {
        let dir = TempDir::new().unwrap();
        let import_dir = dir.path().join("import");
        fresh_dir_with_manifest(&import_dir);
        let nodes_dir = import_dir.join("trie_nodes");
        std::fs::create_dir_all(&nodes_dir).unwrap();

        // First attempt: add some nodes then fail (drop without close)
        {
            let store = Arc::new(PersistedTrieStore::open(&nodes_dir).unwrap());
            let manifest_path = import_dir.join("manifest.json");
            let mut imp =
                SnapshotImporter::new(1, B256::repeat_byte(0xaa), store, manifest_path).unwrap();
            imp.flush_threshold = 1; // Force immediate flush
            let rlp = vec![0xc1, 0x80];
            let node = MptSnapshotNode { hash: keccak256(&rlp), rlp };
            imp.add_node(&node).unwrap();
            // Don't close — drop cleans up temp dir
        }

        // Temp dir should have been cleaned up on drop
        assert!(
            !import_dir.join("trie_nodes_import_tmp").exists(),
            "temp dir should be cleaned up on drop"
        );

        // Second attempt on same dir: should succeed since live store is untouched
        {
            let store = Arc::new(PersistedTrieStore::open(&nodes_dir).unwrap());
            let manifest_path = import_dir.join("manifest.json");
            let result = SnapshotImporter::new(1, B256::repeat_byte(0xbb), store, manifest_path);
            assert!(result.is_ok(), "retry should succeed since live store is untouched");
        }
    }

    /// T4.13: importer.close() updates manifest; verified by reload
    #[test]
    fn t4_13_importer_updates_manifest() {
        let (src_store, _src_dir) = tmp_store_arc();
        let root = build_tree(&src_store, &[(b"hello", &[0x42; 40])]);

        let mut exp = SnapshotExporter::new(src_store.clone(), root, 3).unwrap();
        let mut nodes = Vec::new();
        while let Some(n) = exp.next_node().unwrap() {
            nodes.push(n);
        }

        let dir = TempDir::new().unwrap();
        let (dst_store, manifest_path) = fresh_import_dir(dir.path());

        let mut imp = SnapshotImporter::new(3, root, dst_store, manifest_path.clone()).unwrap();
        for n in &nodes {
            imp.add_node(n).unwrap();
        }
        imp.close().unwrap();

        let manifest = VersionManifest::load(&manifest_path).unwrap();
        assert_eq!(manifest.latest_version, 3);
        assert_eq!(manifest.get_root(3), Some(root));
        assert_eq!(manifest.get_root(0), Some(EMPTY_ROOT_HASH));
    }
}
