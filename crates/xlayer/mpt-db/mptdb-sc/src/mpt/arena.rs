use super::node::MptNode;
use serde::{Deserialize, Serialize};

/// Mutable trie arena for the current block.
/// Phase 1: pure mutable arena, no generation/frozen/hash cache.
#[derive(Clone, Serialize, Deserialize)]
pub struct MutableTrieArena {
    nodes: Vec<MptNode>,
    /// RLP encoding cache aligned with nodes.
    /// After insert/delete, caches on the path from modified node to root must be cleared.
    rlp_cache: Vec<Option<Vec<u8>>>,
    /// Hash cache: keccak256(rlp) for nodes whose RLP >= 32 bytes.
    /// Used to avoid re-entering clean subtrees during encode_child_for_parent.
    hash_cache: Vec<Option<alloy_primitives::B256>>,
    /// Dirty tracking: true if the node was modified since last `clear_all_dirty()`.
    /// New nodes allocated via `alloc()` are dirty by default.
    /// Nodes loaded from persisted storage via `alloc_clean()` are clean.
    dirty: Vec<bool>,
}

impl MutableTrieArena {
    pub fn new() -> Self {
        Self { nodes: Vec::new(), rlp_cache: Vec::new(), hash_cache: Vec::new(), dirty: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(cap),
            rlp_cache: Vec::with_capacity(cap),
            hash_cache: Vec::with_capacity(cap),
            dirty: Vec::with_capacity(cap),
        }
    }

    /// Allocate a new node, returns its index. New nodes are dirty by default.
    pub fn alloc(&mut self, node: MptNode) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        self.rlp_cache.push(None);
        self.hash_cache.push(None);
        self.dirty.push(true);
        idx
    }

    /// Allocate a node that is already persisted (clean). Used when loading from storage.
    pub fn alloc_clean(&mut self, node: MptNode) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        self.rlp_cache.push(None);
        self.hash_cache.push(None);
        self.dirty.push(false);
        idx
    }

    /// Mark a node as dirty (modified in current block).
    pub fn mark_dirty(&mut self, idx: u32) {
        self.dirty[idx as usize] = true;
    }

    /// Check if a node is dirty.
    pub fn is_dirty(&self, idx: u32) -> bool {
        self.dirty[idx as usize]
    }

    /// Clear all dirty flags (call after successful persist).
    pub fn clear_all_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    /// Read a node by index.
    pub fn get(&self, index: u32) -> &MptNode {
        &self.nodes[index as usize]
    }

    /// Mutably read a node (for insert/delete modifications).
    pub fn get_mut(&mut self, index: u32) -> &mut MptNode {
        &mut self.nodes[index as usize]
    }

    /// Get cached RLP for a node.
    pub fn get_rlp(&self, index: u32) -> Option<&Vec<u8>> {
        self.rlp_cache[index as usize].as_ref()
    }

    /// Set cached RLP for a node.
    pub fn set_rlp(&mut self, index: u32, rlp: Vec<u8>) {
        self.rlp_cache[index as usize] = Some(rlp);
    }

    /// Clear cached RLP and hash for a node (cache invalidation on modification).
    pub fn clear_rlp(&mut self, index: u32) {
        self.rlp_cache[index as usize] = None;
        self.hash_cache[index as usize] = None;
    }

    /// Get cached hash for a node (keccak256 of its RLP).
    pub fn get_hash(&self, index: u32) -> Option<alloy_primitives::B256> {
        self.hash_cache[index as usize]
    }

    /// Set cached hash for a node.
    pub fn set_hash(&mut self, index: u32, hash: alloy_primitives::B256) {
        self.hash_cache[index as usize] = Some(hash);
    }

    /// Number of nodes in the arena.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the arena is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for MutableTrieArena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt::node::LeafNode;

    #[test]
    fn t4_1_construction() {
        let a = MutableTrieArena::new();
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());

        let b = MutableTrieArena::with_capacity(100);
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn t4_2_alloc_get_get_mut() {
        let mut arena = MutableTrieArena::new();
        let leaf = MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1, 2, 3]),
            value: vec![0xaa],
        });
        let idx = arena.alloc(leaf);
        assert_eq!(idx, 0);
        assert!(arena.get(idx).is_leaf());

        // get_mut: modify value
        if let MptNode::Leaf(l) = arena.get_mut(idx) {
            l.value = vec![0xbb];
        }
        if let MptNode::Leaf(l) = arena.get(idx) {
            assert_eq!(l.value, vec![0xbb]);
        }
    }

    #[test]
    fn t4_3_rlp_cache() {
        let mut arena = MutableTrieArena::new();
        let idx = arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[]),
            value: vec![],
        }));

        assert!(arena.get_rlp(idx).is_none());

        arena.set_rlp(idx, vec![0xc1, 0x80]);
        assert_eq!(arena.get_rlp(idx).unwrap(), &vec![0xc1, 0x80]);

        arena.clear_rlp(idx);
        assert!(arena.get_rlp(idx).is_none());
    }

    #[test]
    fn t4_4_dirty_tracking() {
        let mut arena = MutableTrieArena::new();
        let idx = arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![1],
        }));
        // New nodes from alloc() are dirty by default
        assert!(arena.is_dirty(idx));

        // alloc_clean produces clean nodes
        let clean_idx = arena.alloc_clean(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[2]),
            value: vec![2],
        }));
        assert!(!arena.is_dirty(clean_idx));

        // mark_dirty
        arena.mark_dirty(clean_idx);
        assert!(arena.is_dirty(clean_idx));

        // clear_all_dirty
        arena.clear_all_dirty();
        assert!(!arena.is_dirty(idx));
        assert!(!arena.is_dirty(clean_idx));
    }

    #[test]
    fn t4_4_len() {
        let mut arena = MutableTrieArena::new();
        assert_eq!(arena.len(), 0);
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[]),
            value: vec![],
        }));
        assert_eq!(arena.len(), 1);
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![1],
        }));
        assert_eq!(arena.len(), 2);
    }
}
