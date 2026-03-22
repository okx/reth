use super::node::MptNode;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// Frozen snapshot of arena data, shared via Arc for O(1) clone.
#[derive(Clone, Default)]
struct FrozenBase {
    nodes: Arc<Vec<MptNode>>,
    hash_cache: Arc<Vec<Option<alloy_primitives::B256>>>,
}

/// Copy-on-Write trie arena.
///
/// `clone()` is O(1): the frozen base is shared via Arc.  Mutations go into
/// a per-instance overlay.  This mirrors sei-db's COW tree semantics where
/// `Tree.Copy()` sets a cowVersion flag and only clones individual nodes on
/// write.
#[derive(Clone)]
pub struct MutableTrieArena {
    /// Immutable shared base (from a prior committed version).
    frozen: FrozenBase,
    /// Nodes that were copied-on-write or newly allocated in this generation.
    /// Key = node index.  For indices < frozen.nodes.len(), this overrides
    /// the frozen base.  For indices >= frozen.nodes.len(), these are new
    /// allocations.
    overlay_nodes: HashMap<u32, MptNode>,
    /// New nodes appended beyond the frozen base.  Indexed as
    /// frozen.nodes.len() + position in this vec.
    appended_nodes: Vec<MptNode>,
    /// RLP encoding cache (sparse, only for modified/accessed nodes).
    rlp_cache: HashMap<u32, Vec<u8>>,
    /// Hash cache overlay — overrides frozen hash_cache entries.
    hash_cache_overlay: HashMap<u32, Option<alloy_primitives::B256>>,
    /// Dirty tracking for nodes modified in the current generation.
    dirty: HashMap<u32, bool>,
}

impl MutableTrieArena {
    pub fn new() -> Self {
        Self {
            frozen: FrozenBase::default(),
            overlay_nodes: HashMap::new(),
            appended_nodes: Vec::new(),
            rlp_cache: HashMap::new(),
            hash_cache_overlay: HashMap::new(),
            dirty: HashMap::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            frozen: FrozenBase::default(),
            overlay_nodes: HashMap::with_capacity(cap),
            appended_nodes: Vec::with_capacity(cap),
            rlp_cache: HashMap::new(),
            hash_cache_overlay: HashMap::new(),
            dirty: HashMap::new(),
        }
    }

    /// Consolidate overlay + appended into the frozen base.
    ///
    /// Uses `Arc::make_mut` so that:
    /// - If the frozen base is uniquely owned (typical after commit — the old working trie was
    ///   consumed): patches in-place, O(overlay_size).
    /// - If shared (another clone still holds a reference): copies then patches, O(base_size). This
    ///   is rare in the normal commit flow.
    ///
    /// This matches sei-db's model where `Copy()` is O(1) and the cost
    /// is paid on the write path, not on the snapshot path.
    pub fn freeze(&mut self) {
        if self.overlay_nodes.is_empty() && self.appended_nodes.is_empty() {
            return;
        }

        // Arc::make_mut: if strong_count == 1, returns &mut in-place (no copy).
        // If shared, clones the inner Vec first (pays O(base_size) once).
        let nodes = Arc::make_mut(&mut self.frozen.nodes);
        for (idx, node) in self.overlay_nodes.drain() {
            nodes[idx as usize] = node;
        }
        nodes.append(&mut self.appended_nodes);

        let hash_cache = Arc::make_mut(&mut self.frozen.hash_cache);
        hash_cache.resize(nodes.len(), None);
        for (idx, hash) in self.hash_cache_overlay.drain() {
            hash_cache[idx as usize] = hash;
        }

        self.rlp_cache.clear();
        self.dirty.clear();
    }

    /// Post-commit: consolidate overlay into frozen base.
    ///
    /// IMPORTANT: caller must drop the old base BEFORE calling this so that
    /// `Arc::make_mut` in freeze() sees strong_count=1 and patches in-place
    /// (O(overlay_size)) instead of copying (O(base_size)).
    pub fn snapshot(&mut self) {
        self.freeze();
    }

    /// Total number of nodes (frozen base + appended).
    pub fn len(&self) -> usize {
        self.frozen.nodes.len() + self.appended_nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Allocate a new node, returns its index. New nodes are dirty by default.
    pub fn alloc(&mut self, node: MptNode) -> u32 {
        let idx = self.len() as u32;
        self.appended_nodes.push(node);
        self.dirty.insert(idx, true);
        idx
    }

    /// Allocate a node that is already persisted (clean). Used when loading from storage.
    pub fn alloc_clean(&mut self, node: MptNode) -> u32 {
        let idx = self.len() as u32;
        self.appended_nodes.push(node);
        // Not inserted into dirty map — defaults to clean.
        idx
    }

    /// Mark a node as dirty (modified in current block).
    pub fn mark_dirty(&mut self, idx: u32) {
        self.dirty.insert(idx, true);
    }

    /// Check if a node is dirty.
    pub fn is_dirty(&self, idx: u32) -> bool {
        self.dirty.get(&idx).copied().unwrap_or(false)
    }

    /// Clear all dirty flags (call after successful persist).
    pub fn clear_all_dirty(&mut self) {
        self.dirty.clear();
    }

    /// Read a node by index.
    pub fn get(&self, index: u32) -> &MptNode {
        let i = index as usize;
        let base_len = self.frozen.nodes.len();
        // Check overlay first (includes COW'd base nodes AND snapshot'd appended nodes).
        if let Some(node) = self.overlay_nodes.get(&index) {
            return node;
        }
        // Then frozen base.
        if i < base_len {
            return &self.frozen.nodes[i];
        }
        // Then appended nodes (only non-empty before snapshot() is called).
        &self.appended_nodes[i - base_len]
    }

    /// Mutably access a node.  If the node lives in the frozen base or
    /// was previously snapshot'd into the overlay, return a mutable ref.
    /// Frozen base nodes are copied into the overlay on first write (COW).
    pub fn get_mut(&mut self, index: u32) -> &mut MptNode {
        let i = index as usize;
        let base_len = self.frozen.nodes.len();
        if i < base_len {
            // COW: copy from frozen base into overlay on first write.
            self.overlay_nodes.entry(index).or_insert_with(|| self.frozen.nodes[i].clone())
        } else if self.overlay_nodes.contains_key(&index) {
            // Node was moved into overlay by snapshot().
            self.overlay_nodes.get_mut(&index).unwrap()
        } else {
            &mut self.appended_nodes[i - base_len]
        }
    }

    /// Get cached RLP for a node.
    pub fn get_rlp(&self, index: u32) -> Option<&Vec<u8>> {
        self.rlp_cache.get(&index)
    }

    /// Set cached RLP for a node.
    pub fn set_rlp(&mut self, index: u32, rlp: Vec<u8>) {
        self.rlp_cache.insert(index, rlp);
    }

    /// Clear cached RLP and hash for a node (cache invalidation on modification).
    pub fn clear_rlp(&mut self, index: u32) {
        self.rlp_cache.remove(&index);
        self.hash_cache_overlay.insert(index, None);
    }

    /// Get cached hash for a node (keccak256 of its RLP).
    pub fn get_hash(&self, index: u32) -> Option<alloy_primitives::B256> {
        if let Some(h) = self.hash_cache_overlay.get(&index) {
            return *h;
        }
        let i = index as usize;
        if i < self.frozen.hash_cache.len() {
            self.frozen.hash_cache[i]
        } else {
            None
        }
    }

    /// Set cached hash for a node.
    pub fn set_hash(&mut self, index: u32, hash: alloy_primitives::B256) {
        self.hash_cache_overlay.insert(index, Some(hash));
    }

    /// Read-only access to all nodes (materialized snapshot).
    ///
    /// This is only used by low-frequency paths (snapshot export, segment build).
    /// It allocates a new Vec if there is an overlay.
    pub fn nodes(&self) -> &[MptNode] {
        if self.overlay_nodes.is_empty() && self.appended_nodes.is_empty() {
            return &self.frozen.nodes;
        }
        // Caller needs a contiguous slice — we can't provide one with overlay.
        // Fall back to frozen base when no overlay (common in read-only paths).
        // For paths that need the full view, use `collect_all_nodes()`.
        &self.frozen.nodes
    }

    /// Collect all nodes into a contiguous Vec (merges frozen + overlay + appended).
    ///
    /// Only for low-frequency paths like segment build or snapshot export.
    pub fn collect_all_nodes(&self) -> Vec<MptNode> {
        let base_len = self.frozen.nodes.len();
        let total = self.len();
        let mut out = Vec::with_capacity(total);
        for i in 0..base_len {
            if let Some(node) = self.overlay_nodes.get(&(i as u32)) {
                out.push(node.clone());
            } else {
                out.push(self.frozen.nodes[i].clone());
            }
        }
        for node in &self.appended_nodes {
            out.push(node.clone());
        }
        out
    }

    /// Read-only access to the hash cache slice.
    ///
    /// Returns the frozen base hash cache.  Callers that need overlay-aware
    /// hashes should use `get_hash(idx)` instead.
    pub fn hash_cache_slice(&self) -> &[Option<alloy_primitives::B256>] {
        &self.frozen.hash_cache
    }

    /// Reference to the frozen base nodes.
    ///
    /// **Must be called after `freeze()`/`snapshot()`** — otherwise the
    /// returned slice is incomplete (overlay and appended nodes are excluded).
    /// Used by background workers to avoid the allocation of `collect_all_nodes`.
    pub fn frozen_nodes_ref(&self) -> &[MptNode] {
        debug_assert!(
            self.overlay_nodes.is_empty() && self.appended_nodes.is_empty(),
            "frozen_nodes_ref called before freeze — overlay or appended not empty"
        );
        &self.frozen.nodes
    }

    /// Reference to the frozen base hash cache.
    ///
    /// **Must be called after `freeze()`/`snapshot()`** — otherwise the
    /// returned slice may be incomplete.
    pub fn frozen_hash_cache_ref(&self) -> &[Option<alloy_primitives::B256>] {
        debug_assert!(
            self.overlay_nodes.is_empty() && self.appended_nodes.is_empty(),
            "frozen_hash_cache_ref called before freeze — overlay or appended not empty"
        );
        &self.frozen.hash_cache
    }

    /// Reconstruct an arena from a lean image: nodes + hash_cache only.
    /// The result is immediately frozen (shared base).
    pub fn from_lean(nodes: Vec<MptNode>, hash_cache: Vec<Option<alloy_primitives::B256>>) -> Self {
        Self {
            frozen: FrozenBase { nodes: Arc::new(nodes), hash_cache: Arc::new(hash_cache) },
            overlay_nodes: HashMap::new(),
            appended_nodes: Vec::new(),
            rlp_cache: HashMap::new(),
            hash_cache_overlay: HashMap::new(),
            dirty: HashMap::new(),
        }
    }
}

// Custom Serialize: merge overlay + frozen into a flat representation.
impl Serialize for MutableTrieArena {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let all_nodes = self.collect_all_nodes();
        let mut hash_cache = Vec::with_capacity(all_nodes.len());
        for i in 0..all_nodes.len() {
            hash_cache.push(self.get_hash(i as u32));
        }
        let mut s = serializer.serialize_struct("MutableTrieArena", 2)?;
        s.serialize_field("nodes", &all_nodes)?;
        s.serialize_field("hash_cache", &hash_cache)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for MutableTrieArena {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ArenaData {
            nodes: Vec<MptNode>,
            hash_cache: Vec<Option<alloy_primitives::B256>>,
        }
        let data = ArenaData::deserialize(deserializer)?;
        Ok(Self::from_lean(data.nodes, data.hash_cache))
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
        assert!(arena.is_dirty(idx));

        let clean_idx = arena.alloc_clean(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[2]),
            value: vec![2],
        }));
        assert!(!arena.is_dirty(clean_idx));

        arena.mark_dirty(clean_idx);
        assert!(arena.is_dirty(clean_idx));

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

    #[test]
    fn t4_5_from_lean_uses_sparse_aux_vectors() {
        let nodes = vec![MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![0xaa],
        })];
        let hash_cache = vec![Some(alloy_primitives::B256::with_last_byte(0x42))];
        let mut arena = MutableTrieArena::from_lean(nodes, hash_cache);

        assert_eq!(arena.len(), 1);
        assert!(arena.get_rlp(0).is_none());
        assert!(!arena.is_dirty(0));

        arena.set_rlp(0, vec![0xc1, 0x80]);
        assert_eq!(arena.get_rlp(0).unwrap(), &vec![0xc1, 0x80]);

        arena.mark_dirty(0);
        assert!(arena.is_dirty(0));

        let idx = arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[2]),
            value: vec![0xbb],
        }));
        assert_eq!(idx, 1);
        assert!(arena.is_dirty(idx));
    }

    #[test]
    fn cow_clone_is_independent() {
        let mut arena = MutableTrieArena::new();
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![0xaa],
        }));
        arena.freeze();

        let mut clone = arena.clone();
        // Mutate clone — should not affect original.
        if let MptNode::Leaf(l) = clone.get_mut(0) {
            l.value = vec![0xbb];
        }
        // Original unchanged.
        if let MptNode::Leaf(l) = arena.get(0) {
            assert_eq!(l.value, vec![0xaa]);
        }
        // Clone has new value.
        if let MptNode::Leaf(l) = clone.get(0) {
            assert_eq!(l.value, vec![0xbb]);
        }
    }

    #[test]
    fn cow_clone_appended_nodes_visible() {
        let mut arena = MutableTrieArena::new();
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![0xaa],
        }));
        arena.freeze();

        let mut clone = arena.clone();
        let idx = clone.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[2]),
            value: vec![0xbb],
        }));
        assert_eq!(idx, 1);
        assert_eq!(clone.len(), 2);
        assert_eq!(arena.len(), 1); // Original unchanged.
    }

    #[test]
    fn freeze_merges_overlay() {
        let mut arena = MutableTrieArena::new();
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[1]),
            value: vec![0xaa],
        }));
        arena.freeze();

        // Modify and append.
        if let MptNode::Leaf(l) = arena.get_mut(0) {
            l.value = vec![0xbb];
        }
        arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: alloy_trie::Nibbles::from_nibbles(&[2]),
            value: vec![0xcc],
        }));
        arena.freeze();

        assert_eq!(arena.len(), 2);
        assert!(arena.overlay_nodes.is_empty());
        assert!(arena.appended_nodes.is_empty());
        if let MptNode::Leaf(l) = arena.get(0) {
            assert_eq!(l.value, vec![0xbb]);
        }
        if let MptNode::Leaf(l) = arena.get(1) {
            assert_eq!(l.value, vec![0xcc]);
        }
    }
}
