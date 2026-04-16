use alloy_primitives::B256;
use alloy_rlp::Decodable;
use alloy_trie::EMPTY_ROOT_HASH;
use mptdb_common::error::{MptDbError, Result};
use std::collections::{HashSet, VecDeque};

use super::{
    encoding::decode_node,
    node::{ChildRef, MptNode},
    persisted::PersistedTrieStore,
    r#trait::MptGcStats,
    stale_index::StaleRootIndex,
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
            .ok_or_else(|| MptDbError::Other(format!("gc: reachable node not found: {hash}")))?;
        let node = decode_node(&rlp)
            .map_err(|e| MptDbError::Other(format!("gc: decode node {hash}: {e}")))?;

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

/// Incremental GC: delete nodes reachable from stale roots but not from live roots.
///
/// # How it works
///
/// 1. Best-effort BFS from all roots in `stale_index` → candidate set. Missing nodes are skipped
///    (they may have been inlined, never persisted, or already cleaned by a previous GC).
/// 2. BFS from all current live roots → live set.
/// 3. Delete `candidates ∖ live`.
/// 4. Remove processed stale index entries before `prune_watermark`.
///
/// This avoids the O(total_nodes) full-scan of `gc_unreachable_nodes` and
/// replaces it with O(stale_path_nodes + live_path_nodes), which is bounded
/// by the current trie size rather than historical accumulated node count.
///
/// Returns `None` if the stale index is empty (nothing to do).
pub(crate) fn gc_incremental(
    store: &PersistedTrieStore,
    stale_index: &StaleRootIndex,
    live_roots: impl IntoIterator<Item = B256>,
    prune_watermark: i64,
) -> Result<Option<MptGcStats>> {
    if stale_index.is_empty()? {
        return Ok(None);
    }

    // Step 1: Best-effort BFS from stale roots → candidates.
    // Missing nodes are tolerated: they were inline, never persisted (WAL-first
    // mode), or already deleted by a previous GC cycle.
    let stale_roots = stale_index.collect_stale_roots()?;
    let candidates = collect_reachable_hashes_tolerant(store, stale_roots)?;

    if candidates.is_empty() {
        // Stale roots had no persisted nodes (e.g. all inline, or WAL-first
        // mode where nodes haven't been written to RocksDB yet).
        stale_index.remove_entries_before(prune_watermark)?;
        return Ok(Some(MptGcStats { scanned_nodes: 0, retained_nodes: 0, deleted_nodes: 0 }));
    }

    // Step 2: BFS from live roots → live set.
    // Missing nodes (None) are tolerated: in WAL-first mode, a live node may not
    // yet be in RocksDB (it's in the segment / WAL).  We conservatively treat it
    // as "live but unresolvable" — we will never delete such a node because it
    // won't appear in `candidates` either (the stale BFS would also skip it).
    //
    // IMPORTANT: decode failures are NOT tolerated here.  A corrupt live node
    // could cause its subtree to be missed from the live set, making intact
    // reachable nodes look like candidates and causing incorrect deletion.
    // A decode error on the live path is a hard error that aborts the GC.
    let live = collect_reachable_hashes_skip_missing(store, live_roots)?;

    // Step 3: stale = candidates - live
    let to_delete: Vec<B256> = candidates.difference(&live).copied().collect();
    let scanned = candidates.len() as u64;
    let deleted = to_delete.len() as u64;
    store.delete_batch_durable(&to_delete)?;

    // Step 4: clean up processed stale index entries
    stale_index.remove_entries_before(prune_watermark)?;

    // If incremental GC found nothing to delete, signal the caller to run a
    // full scan as a safety net.  This covers orphans from rollbacks or other
    // code paths that don't write to the stale index.  When we deleted nodes,
    // the incremental result is authoritative and we skip the full scan.
    if deleted == 0 {
        return Ok(None);
    }

    Ok(Some(MptGcStats {
        scanned_nodes: scanned,
        retained_nodes: scanned - deleted,
        deleted_nodes: deleted,
    }))
}

/// BFS for the **live** set — WAL-first compatible.
///
/// Tolerates nodes that are absent from the persisted store (`get_node` returns
/// `None`): in WAL-first mode a live node may not yet be flushed to RocksDB.
/// An absent live node cannot appear in the candidate set either (stale BFS
/// also skips missing nodes), so there is no risk of incorrect deletion.
///
/// Decode failures are **not** tolerated: a corrupt live node would omit its
/// subtree from the live set, potentially mis-classifying reachable nodes as
/// garbage.  A decode error returns `Err` and aborts the GC.
///
/// Used by both `gc_incremental` (live BFS step) and the legacy full-scan
/// fallback — both need WAL-first compatibility.
pub(crate) fn collect_reachable_hashes_skip_missing(
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
        let rlp = match store.get_node(hash)? {
            Some(r) => r,
            None => continue, // not in RocksDB (WAL-first mode) — skip, not an error
        };
        // Decode failures on live nodes are hard errors: abort GC.
        let node = decode_node(&rlp)
            .map_err(|e| MptDbError::Other(format!("gc: decode live node {hash}: {e}")))?;

        match node {
            MptNode::Leaf(ref leaf) => {
                if let Ok(trie_account) = alloy_trie::TrieAccount::decode(&mut &leaf.value[..]) {
                    let sr = trie_account.storage_root;
                    if sr != EMPTY_ROOT_HASH && live.insert(sr) {
                        queue.push_back(sr);
                    }
                }
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

/// Like `collect_reachable_hashes` but tolerates missing nodes.
///
/// When a node hash is present in a parent's child reference but the node
/// itself is not in the store, the BFS simply skips that branch instead of
/// returning an error.  This is safe for GC's stale-root BFS because:
///
/// - The missing node was inline (not stored separately) and its hash was embedded in the parent's
///   RLP — it has nothing to delete.
/// - The missing node was never persisted (WAL-first mode, still in memory cache only) — it will
///   never be a target for deletion.
/// - The missing node was already deleted by a previous GC cycle — no harm.
fn collect_reachable_hashes_tolerant(
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
        let rlp = match store.get_node(hash)? {
            Some(r) => r,
            None => continue, // tolerate missing nodes
        };
        let node = match decode_node(&rlp) {
            Ok(n) => n,
            Err(_) => continue, // tolerate corrupt nodes
        };

        match node {
            MptNode::Leaf(ref leaf) => {
                if let Ok(trie_account) = alloy_trie::TrieAccount::decode(&mut &leaf.value[..]) {
                    let sr = trie_account.storage_root;
                    if sr != EMPTY_ROOT_HASH && live.insert(sr) {
                        queue.push_back(sr);
                    }
                }
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
///
/// This is the legacy O(total_nodes) full-scan fallback used when the stale
/// index is unavailable.  Prefer `gc_incremental` for production use.
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
        store.persist_batch(&blobs, true).unwrap();
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
        store.persist_batch(&[(fake_root, vec![0xff, 0xfe, 0xfd])], true).unwrap();

        let result = collect_reachable_hashes(&store, [fake_root]);
        assert!(result.is_err());
    }
}
