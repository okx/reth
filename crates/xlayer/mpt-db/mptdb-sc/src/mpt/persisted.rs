use alloy_primitives::B256;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use mptdb_common::error::{MptDbError, Result};
use mptdb_engine::engine::RocksDbEngine;
use mptdb_traits::{
    kv::{KvEngine, KvIterator},
    types::{IterOptions, WriteOptions},
};
use parking_lot::Mutex;
use std::{collections::HashMap, path::Path};

use super::{
    arena::MutableTrieArena,
    encoding::decode_node,
    node::{ChildRef, MptNode},
    tree::MptTree,
};

/// Default maximum number of entries in the node cache before it is cleared.
const DEFAULT_NODE_CACHE_CAPACITY: usize = 100_000;

/// Minimal persisted trie node store backed by RocksDB.
///
/// Key: node_hash (B256, 32 bytes)
/// Value: RLP-encoded node bytes
///
/// An application-level LRU-style cache sits in front of RocksDB to avoid
/// repeated reads for hot nodes (top-level account trie nodes, frequently
/// accessed storage trie roots). The cache uses a simple clear-on-overflow
/// eviction strategy.
pub struct PersistedTrieStore {
    engine: Option<RocksDbEngine>,
    cache: Mutex<HashMap<B256, Vec<u8>>>,
    cache_capacity: usize,
}

impl PersistedTrieStore {
    /// Open a persisted trie store at the given directory with default cache capacity.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_capacity(path, DEFAULT_NODE_CACHE_CAPACITY)
    }

    /// Open a persisted trie store with a custom node cache capacity.
    pub fn open_with_capacity(path: &Path, cache_capacity: usize) -> Result<Self> {
        std::fs::create_dir_all(path)
            .map_err(|e| MptDbError::Other(format!("create trie_nodes dir: {e}")))?;
        let engine = RocksDbEngine::open_plain(path)?;
        Ok(Self { engine: Some(engine), cache: Mutex::new(HashMap::new()), cache_capacity })
    }

    /// Get a node's RLP bytes by its hash.
    ///
    /// Checks the in-memory cache first; on miss, reads from RocksDB and
    /// populates the cache for subsequent lookups.
    pub fn get_node(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        // Fast path: cache hit
        {
            let cache = self.cache.lock();
            if let Some(data) = cache.get(&hash) {
                return Ok(Some(data.clone()));
            }
        }

        // Slow path: read from RocksDB
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        let result = engine.get(hash.as_slice())?;

        // Populate cache on hit
        if let Some(ref data) = result {
            let mut cache = self.cache.lock();
            if cache.len() >= self.cache_capacity {
                cache.clear();
            }
            cache.insert(hash, data.clone());
        }

        Ok(result)
    }

    /// Atomically persist a batch of nodes.
    ///
    /// ## Durability Contract
    ///
    /// When `durable=true`, the write is fsynced before returning. This guarantees
    /// that if the subsequent manifest save succeeds, all referenced nodes are on
    /// stable storage.
    ///
    /// When `durable=false`, the write relies on RocksDB's WAL for crash recovery
    /// but does not force an fsync. This is suitable when the caller manages
    /// durability at a higher level (e.g., grouped commits during snapshot import).
    pub fn persist_batch(&self, nodes: &[(B256, Vec<u8>)], durable: bool) -> Result<()> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        let mut batch = engine.new_batch();
        for (hash, rlp) in nodes {
            batch.set(hash.as_slice(), rlp)?;
        }
        batch.commit(&WriteOptions { sync: durable })?;

        // Populate cache with newly written nodes so subsequent reads are fast
        {
            let mut cache = self.cache.lock();
            for (hash, rlp) in nodes {
                if cache.len() >= self.cache_capacity {
                    cache.clear();
                }
                cache.insert(*hash, rlp.clone());
            }
        }

        Ok(())
    }

    /// Populate the in-memory node cache without writing to disk.
    ///
    /// Used by the async persist path: nodes are cached in memory immediately
    /// so subsequent reads (e.g., `load_tree_from_root`) can find them without
    /// waiting for the background disk write to complete.
    pub fn populate_cache(&self, nodes: &[(B256, Vec<u8>)]) {
        let mut cache = self.cache.lock();
        for (hash, rlp) in nodes {
            if cache.len() >= self.cache_capacity {
                cache.clear();
            }
            cache.insert(*hash, rlp.clone());
        }
    }

    /// Close the store (idempotent).
    pub fn close(&mut self) -> Result<()> {
        if let Some(mut engine) = self.engine.take() {
            engine.close()?;
        }
        Ok(())
    }

    /// Iterate all nodes in the store. Caller must call `first()` to begin.
    pub(crate) fn iter_all_nodes(&self) -> Result<Box<dyn KvIterator>> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        let opts = IterOptions { lower_bound: None, upper_bound: None };
        engine.new_iter(&opts)
    }

    /// Atomically delete a batch of nodes by hash with sync=true.
    pub(crate) fn delete_batch_durable(&self, hashes: &[B256]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        let mut batch = engine.new_batch();
        for hash in hashes {
            batch.delete(hash.as_slice())?;
        }
        batch.commit(&WriteOptions { sync: true })?;

        // Evict deleted nodes from cache
        {
            let mut cache = self.cache.lock();
            for hash in hashes {
                cache.remove(hash);
            }
        }

        Ok(())
    }

    /// Clear the in-memory node cache. Useful for testing or forced reset.
    pub fn clear_cache(&self) {
        self.cache.lock().clear();
    }

    /// Return the current number of entries in the node cache.
    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Check if the store contains no nodes.
    pub(crate) fn is_empty(&self) -> Result<bool> {
        let mut iter = self.iter_all_nodes()?;
        let has_first = iter.first();
        iter.close()?;
        Ok(!has_first)
    }
}

/// Load a complete MptTree from persisted storage, starting from `root`.
///
/// All Hash/Inline children are recursively materialized into Arena nodes.
pub fn load_tree_from_root(store: &PersistedTrieStore, root: B256) -> Result<MptTree> {
    if root == EMPTY_ROOT_HASH {
        return Ok(MptTree::new());
    }

    let root_rlp = store
        .get_node(root)?
        .ok_or_else(|| MptDbError::Other(format!("root node not found: {root}")))?;

    let root_node =
        decode_node(&root_rlp).map_err(|e| MptDbError::Other(format!("decode root node: {e}")))?;

    let mut arena = MutableTrieArena::new();
    let root_idx = materialize_node(store, &mut arena, root_node)?;

    Ok(MptTree { arena, root: Some(root_idx) })
}

/// Load only the paths needed for the provided keys, keeping untouched subtrees
/// as Hash/Inline child refs until they are actually traversed.
pub fn load_tree_paths_from_root(
    store: &PersistedTrieStore,
    root: B256,
    keys: &[Nibbles],
) -> Result<MptTree> {
    if root == EMPTY_ROOT_HASH {
        return Ok(MptTree::new());
    }

    let root_rlp = store
        .get_node(root)?
        .ok_or_else(|| MptDbError::Other(format!("root node not found: {root}")))?;
    let root_node =
        decode_node(&root_rlp).map_err(|e| MptDbError::Other(format!("decode root node: {e}")))?;

    let mut arena = MutableTrieArena::new();
    let root_idx = arena.alloc_clean(root_node);
    let mut tree = MptTree { arena, root: Some(root_idx) };
    for key in keys {
        tree.ensure_path_loaded(store, key)?;
    }
    Ok(tree)
}

/// Recursively materialize a decoded node and all its children into the arena.
fn materialize_node(
    store: &PersistedTrieStore,
    arena: &mut MutableTrieArena,
    node: MptNode,
) -> Result<u32> {
    match node {
        MptNode::Leaf(leaf) => Ok(arena.alloc_clean(MptNode::Leaf(leaf))),
        MptNode::Extension(ext) => {
            let child_idx = materialize_child(store, arena, ext.child)?;
            let ext_node = MptNode::Extension(super::node::ExtensionNode {
                nibbles: ext.nibbles,
                child: ChildRef::Arena(child_idx),
            });
            Ok(arena.alloc_clean(ext_node))
        }
        MptNode::Branch(branch) => {
            let mut new_children: [Option<ChildRef>; 16] = std::array::from_fn(|_| None);
            for (i, child) in branch.children.into_iter().enumerate() {
                if let Some(child_ref) = child {
                    let child_idx = materialize_child(store, arena, child_ref)?;
                    new_children[i] = Some(ChildRef::Arena(child_idx));
                }
            }
            let branch_node = MptNode::Branch(super::node::BranchNode {
                children: new_children,
                value: branch.value,
            });
            Ok(arena.alloc_clean(branch_node))
        }
    }
}

/// Materialize a single ChildRef into an Arena index.
fn materialize_child(
    store: &PersistedTrieStore,
    arena: &mut MutableTrieArena,
    child: ChildRef,
) -> Result<u32> {
    match child {
        ChildRef::Arena(idx) => Ok(idx),
        ChildRef::Hash(hash) => {
            let rlp = store
                .get_node(hash)?
                .ok_or_else(|| MptDbError::Other(format!("child node not found: {hash}")))?;
            let node = decode_node(&rlp)
                .map_err(|e| MptDbError::Other(format!("decode child node: {e}")))?;
            materialize_node(store, arena, node)
        }
        ChildRef::Inline(rlp) => {
            let node = decode_node(&rlp)
                .map_err(|e| MptDbError::Other(format!("decode inline child: {e}")))?;
            materialize_node(store, arena, node)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_trie::Nibbles;
    use tempfile::TempDir;

    fn tmp_store() -> (PersistedTrieStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        (store, dir)
    }

    /// T4.1: persist_batch + get_node roundtrip
    #[test]
    fn t4_1_persist_and_get() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0x11);
        let data = vec![0xc1, 0x80];
        store.persist_batch(&[(hash, data.clone())], true).unwrap();
        let result = store.get_node(hash).unwrap();
        assert_eq!(result, Some(data));
    }

    /// T4.2: duplicate write same hash -> read unchanged
    #[test]
    fn t4_2_duplicate_write() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0x22);
        let data = vec![0xc2, 0x80, 0x80];
        store.persist_batch(&[(hash, data.clone())], true).unwrap();
        store.persist_batch(&[(hash, data.clone())], true).unwrap();
        assert_eq!(store.get_node(hash).unwrap(), Some(data));
    }

    /// T4.3: empty root -> load_tree_from_root returns empty tree
    #[test]
    fn t4_3_empty_root_load() {
        let (store, _dir) = tmp_store();
        let tree = load_tree_from_root(&store, EMPTY_ROOT_HASH).unwrap();
        assert!(tree.is_empty());
    }

    /// T4.4: single leaf persist -> reload -> root_hash unchanged
    #[test]
    fn t4_4_single_leaf_roundtrip() {
        let (store, _dir) = tmp_store();

        let mut tree = MptTree::new();
        let key = Nibbles::unpack(keccak256(b"hello"));
        tree.insert(&key, b"world".to_vec());
        let original_hash = tree.root_hash();

        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let mut reloaded = load_tree_from_root(&store, original_hash).unwrap();
        assert_eq!(reloaded.root_hash(), original_hash);
    }

    /// T4.5: tree with inline child persist -> reload -> root_hash unchanged
    #[test]
    fn t4_5_inline_child_roundtrip() {
        let (store, _dir) = tmp_store();

        let mut tree = MptTree::new();
        // Two keys with shared prefix to create extension + branch with small children
        let k1 = Nibbles::from_nibbles(&[1, 2, 3, 4, 5, 6]);
        let k2 = Nibbles::from_nibbles(&[1, 2, 3, 7, 8, 9]);
        tree.insert(&k1, b"a".to_vec());
        tree.insert(&k2, b"b".to_vec());
        let original_hash = tree.root_hash();

        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let mut reloaded = load_tree_from_root(&store, original_hash).unwrap();
        assert_eq!(reloaded.root_hash(), original_hash);
    }

    /// T4.6: tree with hash child persist -> reload -> root_hash unchanged
    #[test]
    fn t4_6_hash_child_roundtrip() {
        let (store, _dir) = tmp_store();

        let mut tree = MptTree::new();
        // Insert enough keys to create nodes with RLP >= 32 bytes (hashed children)
        for i in 0u8..20 {
            let key = Nibbles::unpack(keccak256(&[i]));
            tree.insert(&key, vec![i; 40]); // large value to force hash refs
        }
        let original_hash = tree.root_hash();

        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let mut reloaded = load_tree_from_root(&store, original_hash).unwrap();
        assert_eq!(reloaded.root_hash(), original_hash);
    }

    /// T4.7: reloaded tree children are all Arena (no residual Hash/Inline)
    #[test]
    fn t4_7_all_arena_children() {
        let (store, _dir) = tmp_store();

        let mut tree = MptTree::new();
        for i in 0u8..10 {
            let key = Nibbles::unpack(keccak256(&[i]));
            tree.insert(&key, vec![i; 40]);
        }
        let root_hash = tree.root_hash();

        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let reloaded = load_tree_from_root(&store, root_hash).unwrap();
        // DFS check: all children must be Arena
        check_all_arena(&reloaded.arena, reloaded.root.unwrap());
    }

    /// T4.7b: path-only load preserves root and materializes fewer nodes than full load.
    #[test]
    fn t4_7b_path_load_roundtrip() {
        let (store, _dir) = tmp_store();

        let mut tree = MptTree::new();
        let mut touched_key = None;
        for i in 0u8..32 {
            let key = Nibbles::unpack(keccak256(&[i]));
            if i == 7 {
                touched_key = Some(key.clone());
            }
            tree.insert(&key, vec![i; 40]);
        }
        let root_hash = tree.root_hash();
        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let key = touched_key.unwrap();
        let mut partial =
            load_tree_paths_from_root(&store, root_hash, std::slice::from_ref(&key)).unwrap();
        let mut full = load_tree_from_root(&store, root_hash).unwrap();

        assert_eq!(partial.root_hash(), root_hash);
        assert_eq!(partial.root_hash(), full.root_hash());
        assert!(partial.arena.len() < full.arena.len());

        partial.insert_from_persisted(&store, &key, b"updated".to_vec()).unwrap();
        full.insert(&key, b"updated".to_vec());
        assert_eq!(partial.root_hash(), full.root_hash());
    }

    fn check_all_arena(arena: &MutableTrieArena, idx: u32) {
        match arena.get(idx) {
            MptNode::Leaf(_) => {}
            MptNode::Extension(ext) => match &ext.child {
                ChildRef::Arena(child_idx) => check_all_arena(arena, *child_idx),
                other => panic!("expected Arena child, got {other:?}"),
            },
            MptNode::Branch(branch) => {
                for child in &branch.children {
                    if let Some(child_ref) = child {
                        match child_ref {
                            ChildRef::Arena(child_idx) => check_all_arena(arena, *child_idx),
                            other => panic!("expected Arena child, got {other:?}"),
                        }
                    }
                }
            }
        }
    }

    /// T4.8: non-empty root but root node missing -> Err
    #[test]
    fn t4_8_missing_root_node() {
        let (store, _dir) = tmp_store();
        let fake_root = B256::repeat_byte(0xaa);
        let result = load_tree_from_root(&store, fake_root);
        assert!(result.is_err());
    }

    /// T4.9: parent references hashed child that is missing -> Err
    #[test]
    fn t4_9_missing_child_node() {
        let (store, _dir) = tmp_store();

        // Create a tree, persist only the root node (not children)
        let mut tree = MptTree::new();
        for i in 0u8..20 {
            let key = Nibbles::unpack(keccak256(&[i]));
            tree.insert(&key, vec![i; 40]);
        }
        let root_hash = tree.root_hash();

        let blobs = tree.collect_node_blobs();
        // Only persist the root blob (first one)
        if let Some(first) = blobs.first() {
            store.persist_batch(&[first.clone()], true).unwrap();
        }

        let result = load_tree_from_root(&store, root_hash);
        assert!(result.is_err());
    }

    /// T4.10: corrupted RLP data -> Err
    #[test]
    fn t4_10_corrupted_rlp() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0xbb);
        // Write garbage data
        store.persist_batch(&[(hash, vec![0xff, 0xfe, 0xfd])], true).unwrap();
        let result = load_tree_from_root(&store, hash);
        assert!(result.is_err());
    }

    /// T4.11: collect_node_blobs includes root, stable across calls
    #[test]
    fn t4_11_collect_node_blobs_stable() {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(keccak256(b"test"));
        tree.insert(&key, b"value".to_vec());
        let root_hash = tree.root_hash();

        let blobs1 = tree.collect_node_blobs();
        let blobs2 = tree.collect_node_blobs();

        assert!(!blobs1.is_empty());
        // Root blob should be present
        assert!(blobs1.iter().any(|(h, _)| *h == root_hash));
        assert_eq!(blobs1.len(), blobs2.len());
        for (a, b) in blobs1.iter().zip(blobs2.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1, b.1);
        }
    }

    /// T4.12: close() can be called multiple times
    #[test]
    fn t4_12_close_idempotent() {
        let (mut store, _dir) = tmp_store();
        store.close().unwrap();
        store.close().unwrap();
    }

    /// T2.1: iter_all_nodes() on empty store returns empty iteration
    #[test]
    fn t2_1_iter_empty() {
        let (store, _dir) = tmp_store();
        let mut iter = store.iter_all_nodes().unwrap();
        assert!(!iter.first());
        iter.close().unwrap();
    }

    /// T2.2: persist then iter_all_nodes() enumerates all nodes
    #[test]
    fn t2_2_iter_after_persist() {
        let (store, _dir) = tmp_store();
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        store.persist_batch(&[(h1, vec![0xc1, 0x80]), (h2, vec![0xc2, 0x80, 0x80])], true).unwrap();

        let mut iter = store.iter_all_nodes().unwrap();
        let mut count = 0;
        if iter.first() {
            count += 1;
            while iter.next() {
                count += 1;
            }
        }
        iter.close().unwrap();
        assert_eq!(count, 2);
    }

    /// T2.3: delete_batch_durable() removes specified nodes, keeps others
    #[test]
    fn t2_3_delete_batch() {
        let (store, _dir) = tmp_store();
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);
        store.persist_batch(&[(h1, vec![0xaa]), (h2, vec![0xbb]), (h3, vec![0xcc])], true).unwrap();

        store.delete_batch_durable(&[h1, h3]).unwrap();
        assert!(store.get_node(h1).unwrap().is_none());
        assert!(store.get_node(h2).unwrap().is_some());
        assert!(store.get_node(h3).unwrap().is_none());
    }

    /// T2.4: delete_batch_durable(empty) -> Ok, no side effects
    #[test]
    fn t2_4_delete_batch_empty() {
        let (store, _dir) = tmp_store();
        store.delete_batch_durable(&[]).unwrap();
    }

    /// T2.5: is_empty() on fresh store -> true
    #[test]
    fn t2_5_is_empty_fresh() {
        let (store, _dir) = tmp_store();
        assert!(store.is_empty().unwrap());
    }

    /// T2.6: is_empty() after write -> false
    #[test]
    fn t2_6_is_empty_after_write() {
        let (store, _dir) = tmp_store();
        store.persist_batch(&[(B256::repeat_byte(0x01), vec![0x80])], true).unwrap();
        assert!(!store.is_empty().unwrap());
    }

    /// T2.7: close then iter/delete/is_empty all Err
    #[test]
    fn t2_7_closed_errors() {
        let (mut store, _dir) = tmp_store();
        store.close().unwrap();
        assert!(store.iter_all_nodes().is_err());
        assert!(store.delete_batch_durable(&[B256::ZERO]).is_err());
        assert!(store.is_empty().is_err());
    }

    // ---- Node cache tests ----

    /// Cache is populated by persist_batch, subsequent get_node hits cache
    #[test]
    fn cache_hit_after_persist() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0xca);
        let data = vec![0xc1, 0x80];
        store.persist_batch(&[(hash, data.clone())], true).unwrap();

        // Cache should contain the node after persist
        assert_eq!(store.cache_len(), 1);
        assert_eq!(store.get_node(hash).unwrap(), Some(data));
    }

    /// Cache miss for an unknown hash returns None
    #[test]
    fn cache_miss_unknown_hash() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0xde);
        assert_eq!(store.get_node(hash).unwrap(), None);
        // Cache should remain empty on miss
        assert_eq!(store.cache_len(), 0);
    }

    /// get_node populates cache on RocksDB hit when cache is empty
    #[test]
    fn cache_populated_by_get_on_miss() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0xab);
        let data = vec![0xc2, 0x80, 0x80];

        // Write directly, then clear cache to simulate cold start
        store.persist_batch(&[(hash, data.clone())], true).unwrap();
        store.clear_cache();
        assert_eq!(store.cache_len(), 0);

        // First get_node should read from RocksDB and populate cache
        assert_eq!(store.get_node(hash).unwrap(), Some(data));
        assert_eq!(store.cache_len(), 1);
    }

    /// Cache is cleared when it exceeds capacity
    #[test]
    fn cache_cleared_on_overflow() {
        let dir = TempDir::new().unwrap();
        // Use a small capacity to make the test fast
        let store = PersistedTrieStore::open_with_capacity(dir.path(), 10).unwrap();

        // Fill cache to capacity via persist_batch
        let mut nodes: Vec<(B256, Vec<u8>)> = Vec::with_capacity(10);
        for i in 0..10u64 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0..8].copy_from_slice(&i.to_le_bytes());
            nodes.push((B256::from(hash_bytes), vec![0xc1, 0x80]));
        }
        store.persist_batch(&nodes, false).unwrap();
        assert_eq!(store.cache_len(), 10);

        // One more persist should trigger clear + insert the new node
        let overflow_hash = B256::repeat_byte(0xff);
        store.persist_batch(&[(overflow_hash, vec![0x80])], false).unwrap();
        // Cache was cleared then 1 entry inserted
        assert_eq!(store.cache_len(), 1);
        assert_eq!(store.get_node(overflow_hash).unwrap(), Some(vec![0x80]));
    }

    /// clear_cache() empties the cache
    #[test]
    fn cache_clear_works() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0x01);
        store.persist_batch(&[(hash, vec![0x80])], true).unwrap();
        assert!(store.cache_len() > 0);
        store.clear_cache();
        assert_eq!(store.cache_len(), 0);
    }

    /// delete_batch_durable evicts deleted nodes from cache
    #[test]
    fn cache_evict_on_delete() {
        let (store, _dir) = tmp_store();
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        store.persist_batch(&[(h1, vec![0xaa]), (h2, vec![0xbb])], true).unwrap();
        assert_eq!(store.cache_len(), 2);

        store.delete_batch_durable(&[h1]).unwrap();
        assert_eq!(store.cache_len(), 1);
        // h1 gone from cache and DB
        assert_eq!(store.get_node(h1).unwrap(), None);
        // h2 still present
        assert_eq!(store.get_node(h2).unwrap(), Some(vec![0xbb]));
    }
}
