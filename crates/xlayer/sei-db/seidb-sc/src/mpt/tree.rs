use alloy_primitives::B256;
use alloy_trie::Nibbles;

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    hash,
    node::{ChildRef, MptNode},
    tree_algo,
};

/// Phase 1: Pure mutable arena MPT, no frozen/generation.
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
                hash::hash_rlp(&rlp)
            }
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

        self.arena.set_rlp(idx, rlp.clone());
        rlp
    }

    /// Layer 2: encode a child reference for embedding in parent.
    fn encode_child_for_parent(&mut self, child: &ChildRef) -> Vec<u8> {
        match child {
            ChildRef::Arena(idx) => {
                let idx = *idx;
                let rlp = self.encode_node(idx);
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
        let node_hash = hash::hash_rlp(&rlp);
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
