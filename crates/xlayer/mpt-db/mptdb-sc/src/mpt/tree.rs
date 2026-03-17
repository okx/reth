use alloy_primitives::B256;
use alloy_trie::Nibbles;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    hash,
    node::{ChildRef, MptNode},
    parallel::ParallelismThresholds,
    tree_algo,
};

/// Phase 1: Pure mutable arena MPT, no frozen/generation.
#[derive(Clone, Serialize, Deserialize)]
pub struct MptTree {
    pub(crate) arena: MutableTrieArena,
    /// Root node index (None = empty trie).
    pub(crate) root: Option<u32>,
}

impl MptTree {
    /// Create an empty trie.
    pub fn new() -> Self {
        Self { arena: MutableTrieArena::new(), root: None }
    }

    /// Insert a key-value pair. Key is in nibbles form (already keccak256'd).
    pub fn insert(&mut self, key: &Nibbles, value: Vec<u8>) {
        let new_root = tree_algo::insert_recursive(&mut self.arena, self.root, key, 0, value);
        self.root = Some(new_root);
    }

    /// Delete a key. Returns true if the key existed.
    pub fn delete(&mut self, key: &Nibbles) -> bool {
        let (deleted, new_root) = tree_algo::delete_recursive(&mut self.arena, self.root, key, 0);
        self.root = new_root;
        deleted
    }

    /// Look up a key. Returns the value if found.
    pub fn get(&self, key: &Nibbles) -> Option<&[u8]> {
        self.get_recursive(self.root?, key, 0)
    }

    /// Compute the root hash.
    /// Empty trie -> EMPTY_ROOT_HASH.
    /// Non-empty  -> keccak256(RLP(root_node)), root is always hashed.
    pub fn root_hash(&mut self) -> B256 {
        match self.root {
            None => alloy_trie::EMPTY_ROOT_HASH,
            Some(root_idx) => {
                let rlp = self.encode_node(root_idx);
                self.arena.get_hash(root_idx).unwrap_or_else(|| hash::hash_rlp(&rlp))
            }
        }
    }

    /// Read-only parallel root hash. Does NOT write rlp_cache.
    ///
    /// If `parallel_frontier_width()` is below the threshold, falls back to
    /// a serial read-only encoding path. The result is byte-for-byte identical
    /// to `root_hash()`.
    pub(crate) fn root_hash_parallel(&self, thresholds: &ParallelismThresholds) -> B256 {
        match self.root {
            None => alloy_trie::EMPTY_ROOT_HASH,
            Some(root_idx) => {
                let fw = self.parallel_frontier_width();
                if thresholds.should_parallelize_account_frontier(fw) {
                    self.root_hash_parallel_inner(root_idx)
                } else {
                    // Serial readonly fallback
                    let rlp = self.encode_node_readonly(root_idx);
                    hash::hash_rlp(&rlp)
                }
            }
        }
    }

    /// Count of independent frontier subtrees suitable for parallel hashing.
    ///
    /// - root = Branch -> count non-empty children
    /// - root = Extension, child = Branch -> count non-empty children of that branch
    /// - otherwise -> 0 (fallback to serial)
    pub(crate) fn parallel_frontier_width(&self) -> usize {
        let root_idx = match self.root {
            Some(idx) => idx,
            None => return 0,
        };
        match self.arena.get(root_idx) {
            MptNode::Branch(branch) => branch.child_count(),
            MptNode::Extension(ext) => match &ext.child {
                ChildRef::Arena(child_idx) => match self.arena.get(*child_idx) {
                    MptNode::Branch(branch) => branch.child_count(),
                    _ => 0,
                },
                _ => 0,
            },
            _ => 0,
        }
    }

    /// Whether the trie is empty.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    // ── Private helpers ──

    fn get_recursive(&self, idx: u32, key: &Nibbles, offset: usize) -> Option<&[u8]> {
        match self.arena.get(idx) {
            MptNode::Leaf(leaf) => {
                let remaining = key.slice(offset..);
                if leaf.nibbles == remaining {
                    Some(&leaf.value)
                } else {
                    None
                }
            }
            MptNode::Extension(ext) => {
                let remaining = key.slice(offset..);
                let ext_len = ext.nibbles.len();
                if remaining.len() >= ext_len && remaining.slice(..ext_len) == ext.nibbles {
                    match &ext.child {
                        ChildRef::Arena(child_idx) => {
                            self.get_recursive(*child_idx, key, offset + ext_len)
                        }
                        _ => panic!("Phase 1: only Arena child refs in live tree"),
                    }
                } else {
                    None
                }
            }
            MptNode::Branch(branch) => {
                if offset >= key.len() {
                    branch.value.as_deref()
                } else {
                    let nibble = key.get_unchecked(offset) as usize;
                    match &branch.children[nibble] {
                        Some(ChildRef::Arena(child_idx)) => {
                            self.get_recursive(*child_idx, key, offset + 1)
                        }
                        Some(_) => panic!("Phase 1: only Arena child refs in live tree"),
                        None => None,
                    }
                }
            }
        }
    }

    /// Layer 1: encode a node by arena index, with caching.
    fn encode_node(&mut self, idx: u32) -> Vec<u8> {
        if let Some(cached) = self.arena.get_rlp(idx) {
            return cached.clone();
        }

        let node = self.arena.get(idx).clone();
        let rlp = match &node {
            MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
            MptNode::Extension(ext) => {
                let child_bytes = self.encode_child_for_parent(&ext.child);
                encode_extension(&ext.nibbles, &child_bytes)
            }
            MptNode::Branch(branch) => {
                let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
                for (i, child) in branch.children.iter().enumerate() {
                    if let Some(c) = child {
                        children_bytes[i] = Some(self.encode_child_for_parent(c));
                    }
                }
                encode_branch(&children_bytes, branch.value.as_deref())
            }
        };

        // Cache both the RLP and its hash for future fast lookups
        if rlp.len() >= 32 {
            self.arena.set_hash(idx, hash::hash_rlp(&rlp));
        }
        self.arena.set_rlp(idx, rlp.clone());
        rlp
    }

    /// Layer 2: encode a child reference for embedding in parent.
    ///
    /// Optimization: if the child is clean and has a cached hash, return the
    /// hash directly without recursing into the subtree. This avoids traversing
    /// clean subtrees during root computation.
    fn encode_child_for_parent(&mut self, child: &ChildRef) -> Vec<u8> {
        match child {
            ChildRef::Arena(idx) => {
                let idx = *idx;
                // Fast path: clean node with cached hash — skip recursion entirely
                if !self.arena.is_dirty(idx) {
                    if let Some(h) = self.arena.get_hash(idx) {
                        return h.to_vec();
                    }
                }
                let rlp = self.encode_node(idx);
                if rlp.len() < 32 {
                    rlp
                } else {
                    let h = hash::hash_rlp(&rlp);
                    self.arena.set_hash(idx, h);
                    h.to_vec()
                }
            }
            ChildRef::Inline(rlp) => rlp.clone(),
            ChildRef::Hash(_) => panic!("Phase 1: Hash child refs not supported in encode"),
        }
    }

    /// Encode a node by arena index without writing to rlp_cache. Read-only.
    fn encode_node_readonly(&self, idx: u32) -> Vec<u8> {
        // Check existing cache first (opportunistic, but never write)
        if let Some(cached) = self.arena.get_rlp(idx) {
            return cached.clone();
        }

        let node = self.arena.get(idx);
        match node {
            MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
            MptNode::Extension(ext) => {
                let child_bytes = self.encode_child_for_parent_readonly(&ext.child);
                encode_extension(&ext.nibbles, &child_bytes)
            }
            MptNode::Branch(branch) => {
                let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
                for (i, child) in branch.children.iter().enumerate() {
                    if let Some(c) = child {
                        children_bytes[i] = Some(self.encode_child_for_parent_readonly(c));
                    }
                }
                encode_branch(&children_bytes, branch.value.as_deref())
            }
        }
    }

    /// Read-only child encoding for parent embedding.
    fn encode_child_for_parent_readonly(&self, child: &ChildRef) -> Vec<u8> {
        match child {
            ChildRef::Arena(idx) => {
                let rlp = self.encode_node_readonly(*idx);
                if rlp.len() < 32 {
                    rlp
                } else {
                    hash::hash_rlp(&rlp).to_vec()
                }
            }
            ChildRef::Inline(rlp) => rlp.clone(),
            ChildRef::Hash(_) => panic!("Phase 1: Hash child refs not supported in encode"),
        }
    }

    /// Parallel root hash inner: find frontier, hash subtrees in parallel, assemble.
    fn root_hash_parallel_inner(&self, root_idx: u32) -> B256 {
        let node = self.arena.get(root_idx);
        match node {
            MptNode::Branch(branch) => {
                // Frontier = branch children; hash each subtree in parallel
                let children_bytes = self.parallel_encode_branch_children(branch);
                let root_rlp = encode_branch(&children_bytes, branch.value.as_deref());
                hash::hash_rlp(&root_rlp)
            }
            MptNode::Extension(ext) => {
                // Extension -> child branch: parallelize the branch children
                match &ext.child {
                    ChildRef::Arena(child_idx) => {
                        let child_node = self.arena.get(*child_idx);
                        match child_node {
                            MptNode::Branch(branch) => {
                                let children_bytes = self.parallel_encode_branch_children(branch);
                                let branch_rlp =
                                    encode_branch(&children_bytes, branch.value.as_deref());
                                // Embed branch into extension
                                let child_embed = if branch_rlp.len() < 32 {
                                    branch_rlp
                                } else {
                                    hash::hash_rlp(&branch_rlp).to_vec()
                                };
                                let root_rlp = encode_extension(&ext.nibbles, &child_embed);
                                hash::hash_rlp(&root_rlp)
                            }
                            _ => {
                                // Should not reach here if parallel_frontier_width was > 0,
                                // but handle gracefully with serial fallback
                                let rlp = self.encode_node_readonly(root_idx);
                                hash::hash_rlp(&rlp)
                            }
                        }
                    }
                    _ => panic!("Phase 1: only Arena child refs in live tree"),
                }
            }
            _ => {
                // Leaf root or other: serial fallback
                let rlp = self.encode_node_readonly(root_idx);
                hash::hash_rlp(&rlp)
            }
        }
    }

    /// Parallel-encode each branch child subtree using rayon, producing
    /// the child-embedding bytes for each slot.
    fn parallel_encode_branch_children(
        &self,
        branch: &super::node::BranchNode,
    ) -> [Option<Vec<u8>>; 16] {
        // Collect (slot_index, arena_idx) for non-empty Arena children
        let tasks: Vec<(usize, u32)> = branch
            .children
            .iter()
            .enumerate()
            .filter_map(|(i, c)| match c {
                Some(ChildRef::Arena(idx)) => Some((i, *idx)),
                Some(ChildRef::Inline(_)) | Some(ChildRef::Hash(_)) => {
                    panic!("Phase 1: only Arena child refs in live tree")
                }
                None => None,
            })
            .collect();

        // Parallel encode each subtree
        let results: Vec<(usize, Vec<u8>)> = tasks
            .into_par_iter()
            .map(|(slot, idx)| {
                let rlp = self.encode_node_readonly(idx);
                let embed = if rlp.len() < 32 { rlp } else { hash::hash_rlp(&rlp).to_vec() };
                (slot, embed)
            })
            .collect();

        let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
        for (slot, bytes) in results {
            children_bytes[slot] = Some(bytes);
        }
        children_bytes
    }

    /// Return root node reference.
    #[allow(dead_code)]
    pub(crate) fn root_node(&self) -> Option<&MptNode> {
        self.root.map(|idx| self.arena.get(idx))
    }

    /// Full DFS export of all `(node_hash, node_rlp)` pairs in this trie.
    ///
    /// Ensures all RLP caches are populated first via `encode_node`.
    /// Phase 2 exports every node (no dirty-tracking optimization).
    pub(crate) fn collect_node_blobs(&mut self) -> Vec<(B256, Vec<u8>)> {
        let root_idx = match self.root {
            Some(idx) => idx,
            None => return vec![],
        };

        // Ensure all RLP caches are populated
        self.encode_node(root_idx);

        let mut result = Vec::new();
        self.collect_blobs_recursive(root_idx, &mut result);
        result
    }

    fn collect_blobs_recursive(&self, idx: u32, out: &mut Vec<(B256, Vec<u8>)>) {
        let rlp = self.arena.get_rlp(idx).expect("RLP cache must be populated").clone();
        let node_hash = self.arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
        out.push((node_hash, rlp));

        let node = self.arena.get(idx);
        match node {
            MptNode::Leaf(_) => {}
            MptNode::Extension(ext) => {
                if let ChildRef::Arena(child_idx) = &ext.child {
                    self.collect_blobs_recursive(*child_idx, out);
                }
            }
            MptNode::Branch(branch) => {
                for child in &branch.children {
                    if let Some(ChildRef::Arena(child_idx)) = child {
                        self.collect_blobs_recursive(*child_idx, out);
                    }
                }
            }
        }
    }

    /// Collect only dirty (modified) node blobs. Requires RLP cache to be populated first
    /// (e.g., via `encode_node`). Returns a subset of what `collect_node_blobs()` would return.
    pub(crate) fn collect_dirty_blobs(&mut self) -> Vec<(B256, Vec<u8>)> {
        let root_idx = match self.root {
            Some(idx) => idx,
            None => return vec![],
        };

        // Ensure all RLP caches are populated
        self.encode_node(root_idx);

        let mut result = Vec::new();
        self.collect_dirty_blobs_recursive(root_idx, &mut result);
        result
    }

    fn collect_dirty_blobs_recursive(&self, idx: u32, out: &mut Vec<(B256, Vec<u8>)>) {
        if !self.arena.is_dirty(idx) {
            // Clean node: all descendants are also clean (dirty propagates from
            // leaf to root on every insert/delete), so skip this entire subtree.
            return;
        }

        let rlp = self.arena.get_rlp(idx).expect("dirty node must have rlp cache").clone();
        let node_hash = self.arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
        out.push((node_hash, rlp));

        // Only recurse into children of dirty nodes
        let node = self.arena.get(idx);
        match node {
            MptNode::Leaf(_) => {}
            MptNode::Extension(ext) => {
                if let ChildRef::Arena(child_idx) = &ext.child {
                    self.collect_dirty_blobs_recursive(*child_idx, out);
                }
            }
            MptNode::Branch(branch) => {
                for child in &branch.children {
                    if let Some(ChildRef::Arena(child_idx)) = child {
                        self.collect_dirty_blobs_recursive(*child_idx, out);
                    }
                }
            }
        }
    }

    /// Compute root hash and collect dirty blobs in a single pass.
    /// The `encode_node` call populates the RLP cache, then we traverse
    /// only dirty nodes to collect their (hash, rlp) pairs.
    pub(crate) fn root_hash_and_dirty_blobs(&mut self) -> (B256, Vec<(B256, Vec<u8>)>) {
        match self.root {
            None => (alloy_trie::EMPTY_ROOT_HASH, vec![]),
            Some(root_idx) => {
                let rlp = self.encode_node(root_idx);
                let root_hash =
                    self.arena.get_hash(root_idx).unwrap_or_else(|| hash::hash_rlp(&rlp));

                let mut blobs = Vec::new();
                self.collect_dirty_blobs_recursive(root_idx, &mut blobs);

                (root_hash, blobs)
            }
        }
    }

    /// Clear all dirty flags in the arena. Call after successful persist.
    pub(crate) fn clear_dirty(&mut self) {
        self.arena.clear_all_dirty();
    }
}

impl Default for MptTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl MptTree {
    #[allow(dead_code)]
    pub(crate) fn arena_ref(&self) -> &MutableTrieArena {
        &self.arena
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_trie::EMPTY_ROOT_HASH;

    fn nibbles_from_bytes(b: &[u8]) -> Nibbles {
        Nibbles::unpack(keccak256(b))
    }

    /// T2.1: empty trie -> parallel root == serial root
    #[test]
    fn t2_1_empty_trie_parallel_eq_serial() {
        let mut tree = MptTree::new();
        let serial = tree.root_hash();
        let tree2 = MptTree::new();
        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let parallel = tree2.root_hash_parallel(&thresholds);
        assert_eq!(serial, parallel);
        assert_eq!(serial, EMPTY_ROOT_HASH);
    }

    /// T2.2: single leaf trie -> parallel root == serial root
    #[test]
    fn t2_2_single_leaf_parallel_eq_serial() {
        let key = nibbles_from_bytes(b"account1");
        let value = b"value1".to_vec();

        let mut tree1 = MptTree::new();
        tree1.insert(&key, value.clone());
        let serial = tree1.root_hash();

        let mut tree2 = MptTree::new();
        tree2.insert(&key, value);
        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let parallel = tree2.root_hash_parallel(&thresholds);
        assert_eq!(serial, parallel);
    }

    /// T2.3: root is branch with multiple subtrees -> parallel root == serial root
    #[test]
    fn t2_3_branch_root_parallel_eq_serial() {
        // Insert enough keys to create a branch at root
        let mut tree1 = MptTree::new();
        let mut tree2 = MptTree::new();
        for i in 0u64..20 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            let value = format!("val{i}").into_bytes();
            tree1.insert(&key, value.clone());
            tree2.insert(&key, value);
        }
        let serial = tree1.root_hash();
        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let parallel = tree2.root_hash_parallel(&thresholds);
        assert_eq!(serial, parallel);
    }

    /// T2.4: root is extension -> child is branch -> parallel root == serial root
    #[test]
    fn t2_4_extension_branch_parallel_eq_serial() {
        // Keys that share a common prefix to create extension -> branch
        let mut tree1 = MptTree::new();
        let mut tree2 = MptTree::new();
        for i in 0u64..30 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            let value = format!("value{i}").into_bytes();
            tree1.insert(&key, value.clone());
            tree2.insert(&key, value);
        }
        let serial = tree1.root_hash();
        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let parallel = tree2.root_hash_parallel(&thresholds);
        assert_eq!(serial, parallel);
    }

    /// T2.5: frontier below threshold -> serial fallback, result still correct
    #[test]
    fn t2_5_frontier_below_threshold_serial_fallback() {
        let mut tree1 = MptTree::new();
        let mut tree2 = MptTree::new();
        for i in 0u64..20 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            let value = format!("val{i}").into_bytes();
            tree1.insert(&key, value.clone());
            tree2.insert(&key, value);
        }
        let serial = tree1.root_hash();
        // Very high threshold forces serial path
        let thresholds =
            ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 10000 };
        let parallel = tree2.root_hash_parallel(&thresholds);
        assert_eq!(serial, parallel);
    }

    /// T2.6: root_hash_parallel() does not write rlp_cache
    #[test]
    fn t2_6_parallel_does_not_write_cache() {
        let mut tree = MptTree::new();
        for i in 0u64..20 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            tree.insert(&key, format!("val{i}").into_bytes());
        }

        // Count how many rlp_cache entries are populated before
        let cache_count_before =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();

        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let _ = tree.root_hash_parallel(&thresholds);

        let cache_count_after =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();

        assert_eq!(cache_count_before, cache_count_after, "parallel hash must not write rlp_cache");
    }

    /// T2.7: root_hash_parallel() followed by insert/delete/get works correctly
    #[test]
    fn t2_7_parallel_then_mutate() {
        let mut tree = MptTree::new();
        let key1 = nibbles_from_bytes(b"key1");
        let key2 = nibbles_from_bytes(b"key2");
        let key3 = nibbles_from_bytes(b"key3");
        tree.insert(&key1, b"v1".to_vec());
        tree.insert(&key2, b"v2".to_vec());

        let thresholds = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        let _ = tree.root_hash_parallel(&thresholds);

        // Insert, delete, get should all work
        tree.insert(&key3, b"v3".to_vec());
        assert_eq!(tree.get(&key3), Some(b"v3".as_ref()));
        assert!(tree.delete(&key1));
        assert!(tree.get(&key1).is_none());
        assert_eq!(tree.get(&key2), Some(b"v2".as_ref()));

        // And serial root_hash should still work
        let hash = tree.root_hash();
        assert_ne!(hash, EMPTY_ROOT_HASH);
    }

    /// T3.1: Insert 10 keys, collect dirty blobs, then insert 1 more.
    /// Second collect_dirty_blobs should export fewer nodes than total.
    #[test]
    fn t3_1_dirty_insert_only_exports_path() {
        let mut tree = MptTree::new();
        for i in 0u8..10 {
            let key = nibbles_from_bytes(&[i]);
            tree.insert(&key, vec![i; 32]);
        }

        // First commit: all nodes are dirty
        let all_blobs = tree.collect_dirty_blobs();
        let total_nodes = tree.collect_node_blobs().len();
        assert_eq!(
            all_blobs.len(),
            total_nodes,
            "first collect_dirty_blobs should export all nodes"
        );

        // Clear dirty flags (simulates successful persist)
        tree.clear_dirty();

        // Insert one more key
        let new_key = nibbles_from_bytes(&[99u8]);
        tree.insert(&new_key, vec![99; 32]);

        let dirty_blobs = tree.collect_dirty_blobs();
        let total_blobs = tree.collect_node_blobs();
        assert!(
            dirty_blobs.len() < total_blobs.len(),
            "dirty blobs ({}) should be fewer than total blobs ({})",
            dirty_blobs.len(),
            total_blobs.len()
        );
        assert!(!dirty_blobs.is_empty(), "inserting a key must produce some dirty blobs");
    }

    /// T3.2: After clear_dirty with no mutations, collect_dirty_blobs returns empty.
    #[test]
    fn t3_2_dirty_empty_commit_exports_nothing() {
        let mut tree = MptTree::new();
        for i in 0u8..5 {
            let key = nibbles_from_bytes(&[i]);
            tree.insert(&key, vec![i; 32]);
        }
        let _ = tree.root_hash();
        tree.clear_dirty();

        let dirty_blobs = tree.collect_dirty_blobs();
        assert!(
            dirty_blobs.is_empty(),
            "no mutations after clear_dirty should produce empty blobs"
        );
    }

    /// T3.3: Insert keys, clear dirty, delete one key, collect_dirty_blobs.
    /// Should only export nodes on the deletion path.
    #[test]
    fn t3_3_dirty_delete_exports_affected_nodes() {
        let mut tree = MptTree::new();
        let mut keys = Vec::new();
        for i in 0u8..10 {
            let key = nibbles_from_bytes(&[i]);
            tree.insert(&key, vec![i; 32]);
            keys.push(key);
        }
        let _ = tree.root_hash();
        tree.clear_dirty();

        // Delete one key
        tree.delete(&keys[0]);

        let dirty_blobs = tree.collect_dirty_blobs();
        let total_blobs = tree.collect_node_blobs();
        assert!(
            dirty_blobs.len() < total_blobs.len(),
            "dirty blobs ({}) should be fewer than total blobs ({}) after single delete",
            dirty_blobs.len(),
            total_blobs.len()
        );
        assert!(!dirty_blobs.is_empty(), "deleting a key must produce some dirty blobs");
    }

    /// T3.4: root_hash_and_dirty_blobs produces same root as root_hash.
    #[test]
    fn t3_4_root_hash_and_dirty_blobs_consistent() {
        let mut tree1 = MptTree::new();
        let mut tree2 = MptTree::new();
        for i in 0u8..20 {
            let key = nibbles_from_bytes(&[i]);
            let val = vec![i; 32];
            tree1.insert(&key, val.clone());
            tree2.insert(&key, val);
        }

        let root1 = tree1.root_hash();
        let (root2, blobs) = tree2.root_hash_and_dirty_blobs();
        assert_eq!(root1, root2);
        assert!(!blobs.is_empty());
    }

    /// T3.5: dirty blobs are a subset of all blobs.
    #[test]
    fn t3_5_dirty_blobs_subset_of_all() {
        let mut tree = MptTree::new();
        for i in 0u8..10 {
            let key = nibbles_from_bytes(&[i]);
            tree.insert(&key, vec![i; 32]);
        }
        let _ = tree.root_hash();
        tree.clear_dirty();

        // Modify a few keys
        let key_mod = nibbles_from_bytes(&[3u8]);
        tree.insert(&key_mod, vec![33; 32]);
        let key_new = nibbles_from_bytes(&[200u8]);
        tree.insert(&key_new, vec![200; 32]);

        let dirty_blobs = tree.collect_dirty_blobs();
        let all_blobs = tree.collect_node_blobs();

        let all_hashes: std::collections::HashSet<B256> =
            all_blobs.iter().map(|(h, _)| *h).collect();
        for (h, _) in &dirty_blobs {
            assert!(all_hashes.contains(h), "dirty blob hash {h} must exist in all blobs");
        }
    }

    /// Warm layer test: rlp_cache survives clear_dirty, and only modified path nodes
    /// have their cache invalidated on subsequent mutations.
    #[test]
    fn test_warm_layer_rlp_cache_survives_clear_dirty() {
        let mut tree = MptTree::new();

        // Step 1: Insert 20 keys and compute root hash (fills rlp_cache for all nodes)
        for i in 0u64..20 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            tree.insert(&key, format!("val{i}").into_bytes());
        }
        let (root1, _blobs1) = tree.root_hash_and_dirty_blobs();
        assert_ne!(root1, EMPTY_ROOT_HASH);

        // Count rlp_cache entries after first root computation
        let cache_count_after_hash =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();
        assert!(cache_count_after_hash > 0, "root_hash_and_dirty_blobs must populate rlp_cache");

        // Step 2: clear_dirty (simulates successful persist)
        tree.clear_dirty();

        // Step 3: Verify rlp_cache entries survived clear_dirty
        let cache_count_after_clear =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();
        assert_eq!(
            cache_count_after_hash, cache_count_after_clear,
            "clear_dirty must NOT clear rlp_cache: before={cache_count_after_hash}, after={cache_count_after_clear}"
        );

        // Step 4: Insert 1 more key (only clears rlp on the affected path)
        let new_key = nibbles_from_bytes(&100u64.to_be_bytes());
        tree.insert(&new_key, b"new_value".to_vec());

        let cache_count_after_insert =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();
        // Some cache entries should have been invalidated on the modified path,
        // but most should survive from the previous computation.
        // The new node itself has no cache yet, and nodes on the path were cleared.
        // So cache_count_after_insert < cache_count_after_clear (some path nodes lost cache)
        // but cache_count_after_insert > 0 (most nodes kept their cache).
        assert!(
            cache_count_after_insert > 0,
            "most rlp_cache entries should survive a single insert"
        );
        // Specifically: at most ~4-5 nodes are on the insertion path (root + branch + ext + leaf),
        // so the vast majority of the original cached entries should still be present.
        let lost = cache_count_after_clear as isize - cache_count_after_insert as isize;
        // We may gain new nodes from structural changes but we should not lose more than
        // a small fraction. The new node doesn't have cache yet but path nodes were invalidated.
        // Allow some slack for structural rearrangements.
        assert!(
            lost < (cache_count_after_clear as isize / 2),
            "inserting one key should not invalidate more than half the cache: lost={lost}, total={cache_count_after_clear}"
        );

        // Step 5: Compute root hash again
        let (root2, blobs2) = tree.root_hash_and_dirty_blobs();

        // Step 6: Root must be correct (different from root1 since we added a key)
        assert_ne!(root2, root1, "root must change after inserting a new key");
        assert!(!blobs2.is_empty(), "dirty blobs should be non-empty for the modified path");

        // Verify the new root is consistent by building a fresh tree and comparing
        let mut fresh_tree = MptTree::new();
        for i in 0u64..20 {
            let key = nibbles_from_bytes(&i.to_be_bytes());
            fresh_tree.insert(&key, format!("val{i}").into_bytes());
        }
        fresh_tree.insert(&new_key, b"new_value".to_vec());
        let fresh_root = fresh_tree.root_hash();
        assert_eq!(root2, fresh_root, "warm-layer root must match a fresh tree built from scratch");

        // Verify that after the second root_hash_and_dirty_blobs, all reachable nodes
        // have rlp_cache populated. Note: the arena may contain orphaned nodes from
        // structural changes (the arena only grows, old nodes are not reclaimed).
        let reachable_count = tree.collect_node_blobs().len();
        let cache_count_final =
            (0..tree.arena.len() as u32).filter(|&i| tree.arena.get_rlp(i).is_some()).count();
        assert!(
            cache_count_final >= reachable_count,
            "after root_hash_and_dirty_blobs, all reachable nodes should have rlp_cache: \
             cached={cache_count_final}, reachable={reachable_count}"
        );
    }
}
