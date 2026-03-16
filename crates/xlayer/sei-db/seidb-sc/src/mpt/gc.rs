use alloy_primitives::B256;
use alloy_rlp::Decodable;
use alloy_trie::EMPTY_ROOT_HASH;
use seidb_common::error::{Result, SeiDbError};
use std::collections::{HashSet, VecDeque};

use super::{
    encoding::decode_node,
    node::{ChildRef, MptNode},
    persisted::PersistedTrieStore,
    r#trait::MptGcStats,
};

/// Collect all node hashes reachable from the given roots via BFS.
///
/// Skips EMPTY_ROOT_HASH roots. Only Hash children are followed;
/// Inline children are not persisted and thus not added to the live set.
pub(crate) fn collect_reachable_hashes(
    store: &PersistedTrieStore,
    roots: impl IntoIterator<Item = B256>,
) -> Result<HashSet<B256>> {
    let mut live = HashSet::new();
    let mut queue = VecDeque::new();

    for root in roots {
        if root != EMPTY_ROOT_HASH && live.insert(root) {
            queue.push_back(root);
        }
    }

    while let Some(hash) = queue.pop_front() {
        let rlp = store
            .get_node(hash)?
            .ok_or_else(|| SeiDbError::Other(format!("gc: reachable node not found: {hash}")))?;
        let node = decode_node(&rlp)
            .map_err(|e| SeiDbError::Other(format!("gc: decode node {hash}: {e}")))?;

        match node {
            MptNode::Leaf(ref leaf) => {
                // Account trie leaves contain TrieAccount RLP with storage_root.
                // We must follow storage_root into the storage trie.
                if let Ok(trie_account) = alloy_trie::TrieAccount::decode(&mut &leaf.value[..]) {
                    let sr = trie_account.storage_root;
                    if sr != EMPTY_ROOT_HASH && live.insert(sr) {
                        queue.push_back(sr);
                    }
                }
                // If the leaf value isn't a valid TrieAccount (e.g., storage trie leaf),
                // that's fine — just skip.
            }
            MptNode::Extension(ext) => {
                if let ChildRef::Hash(h) = ext.child &&
                    live.insert(h)
                {
                    queue.push_back(h);
                }
            }
            MptNode::Branch(branch) => {
                for child in &branch.children {
                    if let Some(ChildRef::Hash(h)) = child &&
                        live.insert(*h)
                    {
                        queue.push_back(*h);
                    }
                }
            }
        }
    }

    Ok(live)
}

/// Delete all persisted nodes whose hash is not in `live`.
/// Returns GC statistics.
pub(crate) fn gc_unreachable_nodes(
    store: &PersistedTrieStore,
    live: &HashSet<B256>,
) -> Result<MptGcStats> {
    let mut scanned: u64 = 0;
    let mut to_delete = Vec::new();

    let mut iter = store.iter_all_nodes()?;
    if iter.first() {
        loop {
            scanned += 1;
            let key = iter.key();
            if key.len() == 32 {
                let hash = B256::from_slice(key);
                if !live.contains(&hash) {
                    to_delete.push(hash);
                }
            }
            if !iter.next() {
                break;
            }
        }
    }
    iter.close()?;

    let deleted = to_delete.len() as u64;
    store.delete_batch_durable(&to_delete)?;

    Ok(MptGcStats {
        scanned_nodes: scanned,
        retained_nodes: scanned - deleted,
        deleted_nodes: deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_trie::Nibbles;
    use tempfile::TempDir;

    use crate::mpt::tree::MptTree;

    fn tmp_store() -> (PersistedTrieStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        (store, dir)
    }

    /// Build a tree, persist it, and return the root hash.
    fn build_tree(store: &PersistedTrieStore, entries: &[(&[u8], &[u8])]) -> B256 {
        let mut tree = MptTree::new();
        for (k, v) in entries {
            let key = Nibbles::unpack(keccak256(k));
            tree.insert(&key, v.to_vec());
        }
        let root = tree.root_hash();
        let blobs = tree.collect_node_blobs();
        store.persist_batch_durable(&blobs).unwrap();
        root
    }

    /// T5.1: collect_reachable_hashes(empty roots) -> empty set
    #[test]
    fn t5_1_empty_roots() {
        let (store, _dir) = tmp_store();
        let live = collect_reachable_hashes(&store, std::iter::empty()).unwrap();
        assert!(live.is_empty());
    }

    /// T5.2: collect_reachable_hashes(single root) -> includes root and all children
    #[test]
    fn t5_2_single_root() {
        let (store, _dir) = tmp_store();
        let root = build_tree(&store, &[(b"key1", b"val1"), (b"key2", b"val2")]);
        let live = collect_reachable_hashes(&store, [root]).unwrap();
        assert!(live.contains(&root));
        assert!(!live.is_empty());
    }

    /// T5.3: collect_reachable_hashes(multiple roots sharing nodes) -> live set deduped
    #[test]
    fn t5_3_shared_nodes() {
        let (store, _dir) = tmp_store();
        // Two trees that share common entries
        let root1 = build_tree(&store, &[(b"a", b"1"), (b"b", b"2")]);
        let root2 = build_tree(&store, &[(b"a", b"1"), (b"c", b"3")]);
        let live = collect_reachable_hashes(&store, [root1, root2]).unwrap();
        assert!(live.contains(&root1));
        assert!(live.contains(&root2));
    }

    /// T5.4: gc_unreachable_nodes() with no orphans -> deleted_nodes=0
    #[test]
    fn t5_4_no_orphans() {
        let (store, _dir) = tmp_store();
        let root = build_tree(&store, &[(b"x", b"y")]);
        let live = collect_reachable_hashes(&store, [root]).unwrap();
        let stats = gc_unreachable_nodes(&store, &live).unwrap();
        assert_eq!(stats.deleted_nodes, 0);
        assert!(stats.scanned_nodes > 0);
        assert_eq!(stats.retained_nodes, stats.scanned_nodes);
    }

    /// T5.5: prune then gc deletes orphan nodes
    #[test]
    fn t5_5_gc_deletes_orphans() {
        let (store, _dir) = tmp_store();
        // Tree v1
        let _root1 = build_tree(&store, &[(b"old_key", &[0xaa; 40])]);
        // Tree v2 — completely different
        let root2 = build_tree(&store, &[(b"new_key", &[0xbb; 40])]);

        // Only root2 is "live" (root1 was pruned from manifest)
        let live = collect_reachable_hashes(&store, [root2]).unwrap();
        let stats = gc_unreachable_nodes(&store, &live).unwrap();
        assert!(stats.deleted_nodes > 0, "should have deleted orphan nodes from v1");
    }

    /// T5.6: gc does not delete nodes still referenced by retained versions
    #[test]
    fn t5_6_gc_preserves_retained() {
        let (store, _dir) = tmp_store();
        let root1 = build_tree(&store, &[(b"k1", &[0xaa; 40])]);
        let root2 = build_tree(&store, &[(b"k2", &[0xbb; 40])]);

        // Both roots are live
        let live = collect_reachable_hashes(&store, [root1, root2]).unwrap();
        let stats = gc_unreachable_nodes(&store, &live).unwrap();
        assert_eq!(stats.deleted_nodes, 0);

        // Verify both roots still accessible
        assert!(store.get_node(root1).unwrap().is_some());
        assert!(store.get_node(root2).unwrap().is_some());
    }

    /// T5.7: corrupt persisted node during mark -> Err
    #[test]
    fn t5_7_corrupt_node() {
        let (store, _dir) = tmp_store();
        let fake_root = B256::repeat_byte(0xab);
        // Write garbage data under a valid-looking hash
        store.persist_batch_durable(&[(fake_root, vec![0xff, 0xfe, 0xfd])]).unwrap();

        let result = collect_reachable_hashes(&store, [fake_root]);
        assert!(result.is_err());
    }
}
