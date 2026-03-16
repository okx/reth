use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use mptdb_common::error::{MptDbError, Result};
use mptdb_engine::engine::RocksDbEngine;
use mptdb_traits::{
    kv::{KvEngine, KvIterator},
    types::{IterOptions, WriteOptions},
};
use std::path::Path;

use super::{
    arena::MutableTrieArena,
    encoding::decode_node,
    node::{ChildRef, MptNode},
    tree::MptTree,
};

/// Minimal persisted trie node store backed by RocksDB.
///
/// Key: node_hash (B256, 32 bytes)
/// Value: RLP-encoded node bytes
pub struct PersistedTrieStore {
    engine: Option<RocksDbEngine>,
}

impl PersistedTrieStore {
    /// Open a persisted trie store at the given directory.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .map_err(|e| MptDbError::Other(format!("create trie_nodes dir: {e}")))?;
        let engine = RocksDbEngine::open_plain(path)?;
        Ok(Self { engine: Some(engine) })
    }

    /// Get a node's RLP bytes by its hash.
    pub fn get_node(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        engine.get(hash.as_slice())
    }

    /// Atomically persist a batch of nodes with sync=true.
    pub fn persist_batch_durable(&self, nodes: &[(B256, Vec<u8>)]) -> Result<()> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| MptDbError::Other("PersistedTrieStore is closed".to_string()))?;
        let mut batch = engine.new_batch();
        for (hash, rlp) in nodes {
            batch.set(hash.as_slice(), rlp)?;
        }
        batch.commit(&WriteOptions { sync: true })?;
        Ok(())
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
        Ok(())
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

/// Recursively materialize a decoded node and all its children into the arena.
fn materialize_node(
    store: &PersistedTrieStore,
    arena: &mut MutableTrieArena,
    node: MptNode,
) -> Result<u32> {
    match node {
        MptNode::Leaf(leaf) => Ok(arena.alloc(MptNode::Leaf(leaf))),
        MptNode::Extension(ext) => {
            let child_idx = materialize_child(store, arena, ext.child)?;
            let ext_node = MptNode::Extension(super::node::ExtensionNode {
                nibbles: ext.nibbles,
                child: ChildRef::Arena(child_idx),
            });
            Ok(arena.alloc(ext_node))
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
            Ok(arena.alloc(branch_node))
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

    /// T4.1: persist_batch_durable + get_node roundtrip
    #[test]
    fn t4_1_persist_and_get() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0x11);
        let data = vec![0xc1, 0x80];
        store.persist_batch_durable(&[(hash, data.clone())]).unwrap();
        let result = store.get_node(hash).unwrap();
        assert_eq!(result, Some(data));
    }

    /// T4.2: duplicate write same hash -> read unchanged
    #[test]
    fn t4_2_duplicate_write() {
        let (store, _dir) = tmp_store();
        let hash = B256::repeat_byte(0x22);
        let data = vec![0xc2, 0x80, 0x80];
        store.persist_batch_durable(&[(hash, data.clone())]).unwrap();
        store.persist_batch_durable(&[(hash, data.clone())]).unwrap();
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
        store.persist_batch_durable(&blobs).unwrap();

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
        store.persist_batch_durable(&blobs).unwrap();

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
        store.persist_batch_durable(&blobs).unwrap();

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
        store.persist_batch_durable(&blobs).unwrap();

        let reloaded = load_tree_from_root(&store, root_hash).unwrap();
        // DFS check: all children must be Arena
        check_all_arena(&reloaded.arena, reloaded.root.unwrap());
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
            store.persist_batch_durable(&[first.clone()]).unwrap();
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
        store.persist_batch_durable(&[(hash, vec![0xff, 0xfe, 0xfd])]).unwrap();
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
        store
            .persist_batch_durable(&[(h1, vec![0xc1, 0x80]), (h2, vec![0xc2, 0x80, 0x80])])
            .unwrap();

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
        store
            .persist_batch_durable(&[(h1, vec![0xaa]), (h2, vec![0xbb]), (h3, vec![0xcc])])
            .unwrap();

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
        store.persist_batch_durable(&[(B256::repeat_byte(0x01), vec![0x80])]).unwrap();
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
}
