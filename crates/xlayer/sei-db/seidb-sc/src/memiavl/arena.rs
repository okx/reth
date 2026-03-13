//! Arena-based node storage for MemIAVL trees.
//!
//! Replaces `Arc<Node>` with index-based references to eliminate per-node
//! heap allocations and atomic reference counting in the hot path.
//!
//! Three storage tiers:
//! - **Persisted (gen=0)**: mmap-backed snapshot nodes via `PersistedNode`
//! - **Frozen (gen=1..N)**: immutable `Arc<FrozenArena>` shared with snapshot copies
//! - **Mutable (gen=current)**: `MutableArena` owned exclusively by the active tree

use crate::memiavl::node::MemNode;
use std::sync::OnceLock;

/// Index into the arena system. Encodes which generation (storage tier) the
/// node belongs to and its position within that tier.
///
/// - `gen == 0`: persisted node from mmap snapshot
/// - `gen == 1..N`: frozen arena (immutable, shared via Arc)
/// - `gen == current_gen`: mutable arena (current block's allocations)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeIdx {
    pub generation: u16,
    pub index: u32,
}

/// Sentinel generation for persisted (mmap) nodes.
pub const GEN_PERSISTED: u16 = 0;

impl NodeIdx {
    /// Create an index pointing to a persisted snapshot node.
    /// `index` is the PersistedNode index (branch or leaf).
    /// `is_leaf` is encoded in the high bit of `index`.
    #[inline]
    pub fn persisted(index: u32, is_leaf: bool) -> Self {
        let index = if is_leaf { index | PERSISTED_LEAF_BIT } else { index };
        Self { generation: GEN_PERSISTED, index }
    }

    /// Create an index pointing to a MemNode in a specific generation's arena.
    #[inline]
    pub fn mem(generation: u16, index: u32) -> Self {
        debug_assert!(generation > 0, "generation 0 is reserved for persisted nodes");
        Self { generation, index }
    }

    /// Returns true if this points to a persisted (mmap) node.
    #[inline]
    pub fn is_persisted(self) -> bool {
        self.generation == GEN_PERSISTED
    }

    /// For persisted nodes: returns the raw index (without the leaf bit).
    #[inline]
    pub fn persisted_index(self) -> u32 {
        self.index & !PERSISTED_LEAF_BIT
    }

    /// For persisted nodes: returns whether this is a leaf node.
    #[inline]
    pub fn persisted_is_leaf(self) -> bool {
        self.index & PERSISTED_LEAF_BIT != 0
    }
}

const PERSISTED_LEAF_BIT: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// MutableArena — append-only allocator for the current block
// ---------------------------------------------------------------------------

/// Growable node storage for the current tree version's mutations.
/// Nodes are appended via `alloc()` during `set_recursive` / `remove_recursive`.
pub struct MutableArena {
    nodes: Vec<MemNode>,
}

impl MutableArena {
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { nodes: Vec::with_capacity(cap) }
    }

    /// Allocate a new node, returning its index within this arena.
    #[inline]
    pub fn alloc(&mut self, node: MemNode) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        idx
    }

    #[inline]
    pub fn get(&self, index: u32) -> &MemNode {
        &self.nodes[index as usize]
    }

    #[inline]
    pub fn get_mut(&mut self, index: u32) -> &mut MemNode {
        &mut self.nodes[index as usize]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Freeze this arena into an immutable `FrozenArena`.
    pub fn freeze(self) -> FrozenArena {
        FrozenArena { nodes: self.nodes.into_boxed_slice() }
    }
}

impl Default for MutableArena {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FrozenArena — immutable snapshot shared via Arc
// ---------------------------------------------------------------------------

/// Immutable node storage, created by freezing a `MutableArena`.
/// Shared between the original tree and its snapshot copies via `Arc`.
pub struct FrozenArena {
    nodes: Box<[MemNode]>,
}

impl FrozenArena {
    #[inline]
    pub fn get(&self, index: u32) -> &MemNode {
        &self.nodes[index as usize]
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

// ---------------------------------------------------------------------------
// CoW helpers
// ---------------------------------------------------------------------------

/// Materialize a node into the mutable arena for mutation.
///
/// If the node is already in the current mutable arena AND its version is
/// newer than `cow_version`, it can be mutated in place (returns the same
/// index). Otherwise, the node data is copied to a new slot in the mutable
/// arena.
///
/// For persisted nodes (gen==0), the caller must use `cow_persisted_to_mutable`
/// instead.
///
/// Returns the arena index in the mutable arena.
pub fn cow_to_mutable(
    arena: &mut MutableArena,
    current_gen: u16,
    frozen: &[std::sync::Arc<FrozenArena>],
    idx: NodeIdx,
    version: u32,
    cow_version: u32,
) -> u32 {
    if idx.generation == current_gen {
        let node = arena.get(idx.index);
        if node.version > cow_version {
            // Safe to mutate in place — this node was created in a version
            // after the last copy(), so no snapshot shares it.
            // Clear hash and update version (version is part of the hash input).
            let node = arena.get_mut(idx.index);
            node.hash = OnceLock::new();
            node.version = version;
            return idx.index;
        }
        // Same generation but protected by cow_version — must copy.
        let mut cloned = arena.get(idx.index).clone();
        cloned.hash = OnceLock::new();
        cloned.version = version;
        arena.alloc(cloned)
    } else {
        // Node is in a frozen arena — must copy to mutable.
        debug_assert!(!idx.is_persisted(), "persisted nodes should be handled separately");
        let frozen_arena = &frozen[(idx.generation - 1) as usize];
        let mut cloned = frozen_arena.get(idx.index).clone();
        cloned.hash = OnceLock::new();
        cloned.version = version;
        arena.alloc(cloned)
    }
}

/// Resolve a NodeIdx to a reference to a MemNode.
///
/// Panics if the index is persisted (gen==0). Use `resolve_node` for a
/// safe variant that also handles persisted nodes via snapshot.
pub fn resolve_mem_node<'a>(
    arena: &'a MutableArena,
    frozen: &'a [std::sync::Arc<FrozenArena>],
    current_gen: u16,
    idx: NodeIdx,
) -> &'a MemNode {
    debug_assert!(!idx.is_persisted(), "cannot resolve persisted node as MemNode");
    if idx.generation == current_gen {
        arena.get(idx.index)
    } else {
        let frozen_arena = &frozen[(idx.generation - 1) as usize];
        frozen_arena.get(idx.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_idx_persisted() {
        let leaf = NodeIdx::persisted(42, true);
        assert!(leaf.is_persisted());
        assert!(leaf.persisted_is_leaf());
        assert_eq!(leaf.persisted_index(), 42);

        let branch = NodeIdx::persisted(99, false);
        assert!(branch.is_persisted());
        assert!(!branch.persisted_is_leaf());
        assert_eq!(branch.persisted_index(), 99);
    }

    #[test]
    fn test_node_idx_mem() {
        let idx = NodeIdx::mem(3, 100);
        assert!(!idx.is_persisted());
        assert_eq!(idx.generation, 3);
        assert_eq!(idx.index, 100);
    }

    #[test]
    fn test_mutable_arena_alloc() {
        let mut arena = MutableArena::new();
        let node = MemNode::new_leaf_node(b"k".to_vec(), b"v".to_vec(), 1);
        let idx = arena.alloc(node);
        assert_eq!(idx, 0);
        assert_eq!(arena.get(0).key, b"k");

        let node2 = MemNode::new_leaf_node(b"k2".to_vec(), b"v2".to_vec(), 1);
        let idx2 = arena.alloc(node2);
        assert_eq!(idx2, 1);
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn test_freeze_and_read() {
        let mut arena = MutableArena::new();
        arena.alloc(MemNode::new_leaf_node(b"a".to_vec(), b"1".to_vec(), 1));
        arena.alloc(MemNode::new_leaf_node(b"b".to_vec(), b"2".to_vec(), 1));

        let frozen = arena.freeze();
        assert_eq!(frozen.get(0).key, b"a");
        assert_eq!(frozen.get(1).key, b"b");
        assert_eq!(frozen.len(), 2);
    }
}
