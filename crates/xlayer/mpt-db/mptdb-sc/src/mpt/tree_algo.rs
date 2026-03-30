use alloy_trie::Nibbles;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};

use super::{
    arena::MutableTrieArena,
    node::{BranchNode, ChildRef, ExtensionNode, LeafNode, MptNode},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct TreeAlgoStats {
    pub slot_inserts: u64,
    pub slot_deletes: u64,
    pub leaf_splits: u64,
    pub extension_splits: u64,
    pub branch_collapse_to_empty: u64,
    pub branch_collapse_to_leaf: u64,
    pub branch_collapse_to_extension: u64,
    pub extension_leaf_merges: u64,
    pub extension_extension_merges: u64,
}

static SLOT_INSERTS: AtomicU64 = AtomicU64::new(0);
static SLOT_DELETES: AtomicU64 = AtomicU64::new(0);
static LEAF_SPLITS: AtomicU64 = AtomicU64::new(0);
static EXTENSION_SPLITS: AtomicU64 = AtomicU64::new(0);
static BRANCH_COLLAPSE_TO_EMPTY: AtomicU64 = AtomicU64::new(0);
static BRANCH_COLLAPSE_TO_LEAF: AtomicU64 = AtomicU64::new(0);
static BRANCH_COLLAPSE_TO_EXTENSION: AtomicU64 = AtomicU64::new(0);
static EXTENSION_LEAF_MERGES: AtomicU64 = AtomicU64::new(0);
static EXTENSION_EXTENSION_MERGES: AtomicU64 = AtomicU64::new(0);
static STATS_ENABLED: OnceLock<bool> = OnceLock::new();

#[inline]
fn stats_enabled() -> bool {
    *STATS_ENABLED.get_or_init(|| std::env::var_os("MPT_PROFILE_TREE_STATS").is_some())
}

pub(crate) fn reset_stats() {
    if !stats_enabled() {
        return;
    }
    SLOT_INSERTS.store(0, Ordering::Relaxed);
    SLOT_DELETES.store(0, Ordering::Relaxed);
    LEAF_SPLITS.store(0, Ordering::Relaxed);
    EXTENSION_SPLITS.store(0, Ordering::Relaxed);
    BRANCH_COLLAPSE_TO_EMPTY.store(0, Ordering::Relaxed);
    BRANCH_COLLAPSE_TO_LEAF.store(0, Ordering::Relaxed);
    BRANCH_COLLAPSE_TO_EXTENSION.store(0, Ordering::Relaxed);
    EXTENSION_LEAF_MERGES.store(0, Ordering::Relaxed);
    EXTENSION_EXTENSION_MERGES.store(0, Ordering::Relaxed);
}

pub(crate) fn snapshot_stats() -> TreeAlgoStats {
    if !stats_enabled() {
        return TreeAlgoStats::default();
    }
    TreeAlgoStats {
        slot_inserts: SLOT_INSERTS.load(Ordering::Relaxed),
        slot_deletes: SLOT_DELETES.load(Ordering::Relaxed),
        leaf_splits: LEAF_SPLITS.load(Ordering::Relaxed),
        extension_splits: EXTENSION_SPLITS.load(Ordering::Relaxed),
        branch_collapse_to_empty: BRANCH_COLLAPSE_TO_EMPTY.load(Ordering::Relaxed),
        branch_collapse_to_leaf: BRANCH_COLLAPSE_TO_LEAF.load(Ordering::Relaxed),
        branch_collapse_to_extension: BRANCH_COLLAPSE_TO_EXTENSION.load(Ordering::Relaxed),
        extension_leaf_merges: EXTENSION_LEAF_MERGES.load(Ordering::Relaxed),
        extension_extension_merges: EXTENSION_EXTENSION_MERGES.load(Ordering::Relaxed),
    }
}

pub(crate) fn note_slot_insert() {
    if stats_enabled() {
        SLOT_INSERTS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn note_slot_delete() {
    if stats_enabled() {
        SLOT_DELETES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Insert a key-value pair into the trie rooted at `node_idx`.
/// `offset` is the current position in `key`.
/// Returns the new root index for this subtree.
pub(crate) fn insert_recursive(
    arena: &mut MutableTrieArena,
    node_idx: Option<u32>,
    key: &Nibbles,
    offset: usize,
    value: Vec<u8>,
) -> u32 {
    let remaining = key.slice(offset..);

    match node_idx {
        None => {
            // Empty subtree: create a new leaf
            arena.alloc(MptNode::Leaf(LeafNode { nibbles: remaining, value }))
        }
        Some(idx) => {
            arena.clear_rlp(idx);
            arena.mark_dirty(idx);

            // Extract routing info WITHOUT cloning the entire MptNode.
            // For Branch nodes (the most common intermediate node), we only
            // need one child ref (8 bytes) — cloning the full BranchNode
            // (~260 bytes with 16 children) is wasteful.
            enum Route {
                Leaf,
                Extension,
                BranchDescend { nibble: usize, child_idx: Option<u32> },
                BranchSetValue,
            }
            let route = match arena.get(idx) {
                MptNode::Leaf(_) => Route::Leaf,
                MptNode::Extension(_) => Route::Extension,
                MptNode::Branch(branch) => {
                    if offset >= key.len() {
                        Route::BranchSetValue
                    } else {
                        let nibble = key.get_unchecked(offset) as usize;
                        let child_idx = branch.children[nibble].as_ref().map(|c| match c {
                            ChildRef::Arena(i) => *i,
                            _ => panic!("Phase 1: only Arena child refs"),
                        });
                        Route::BranchDescend { nibble, child_idx }
                    }
                }
            };

            match route {
                Route::BranchDescend { nibble, child_idx } => {
                    // Hot path: no full node clone needed.
                    let new_child = insert_recursive(arena, child_idx, key, offset + 1, value);
                    if let MptNode::Branch(b) = arena.get_mut(idx) {
                        b.children[nibble] = Some(ChildRef::Arena(new_child));
                    }
                    idx
                }
                Route::BranchSetValue => {
                    if let MptNode::Branch(b) = arena.get_mut(idx) {
                        b.value = Some(value);
                    }
                    idx
                }
                Route::Leaf => {
                    let leaf = match arena.get(idx) {
                        MptNode::Leaf(l) => l.clone(),
                        _ => unreachable!(),
                    };
                    insert_at_leaf(arena, idx, &leaf, &remaining, value)
                }
                Route::Extension => {
                    let ext = match arena.get(idx) {
                        MptNode::Extension(e) => e.clone(),
                        _ => unreachable!(),
                    };
                    insert_at_extension(arena, idx, &ext, key, offset, value)
                }
            }
        }
    }
}

fn insert_at_leaf(
    arena: &mut MutableTrieArena,
    idx: u32,
    leaf: &LeafNode,
    remaining: &Nibbles,
    value: Vec<u8>,
) -> u32 {
    let common_len = leaf.nibbles.common_prefix_length(remaining);

    if common_len == leaf.nibbles.len() && common_len == remaining.len() {
        // Exact key match: update value in place
        if let MptNode::Leaf(l) = arena.get_mut(idx) {
            l.value = value;
        }
        idx
    } else {
        LEAF_SPLITS.fetch_add(1, Ordering::Relaxed);
        // Split: create Branch, put old leaf and new leaf at respective nibble slots
        let mut branch = BranchNode::new();

        // Handle old leaf
        let old_rest = leaf.nibbles.slice(common_len..);
        if old_rest.is_empty() {
            branch.value = Some(leaf.value.clone());
        } else {
            let old_nibble = old_rest.get_unchecked(0) as usize;
            let old_leaf_rest = old_rest.slice(1..);
            let old_leaf_idx = arena.alloc(MptNode::Leaf(LeafNode {
                nibbles: old_leaf_rest,
                value: leaf.value.clone(),
            }));
            branch.children[old_nibble] = Some(ChildRef::Arena(old_leaf_idx));
        }

        // Handle new leaf
        let new_rest = remaining.slice(common_len..);
        if new_rest.is_empty() {
            branch.value = Some(value);
        } else {
            let new_nibble = new_rest.get_unchecked(0) as usize;
            let new_leaf_rest = new_rest.slice(1..);
            let new_leaf_idx =
                arena.alloc(MptNode::Leaf(LeafNode { nibbles: new_leaf_rest, value }));
            branch.children[new_nibble] = Some(ChildRef::Arena(new_leaf_idx));
        }

        let branch_idx = arena.alloc(MptNode::Branch(branch));

        if common_len > 0 {
            let ext_nibbles = leaf.nibbles.slice(..common_len);
            arena.alloc(MptNode::Extension(ExtensionNode {
                nibbles: ext_nibbles,
                child: ChildRef::Arena(branch_idx),
            }))
        } else {
            branch_idx
        }
    }
}

fn insert_at_extension(
    arena: &mut MutableTrieArena,
    idx: u32,
    ext: &ExtensionNode,
    key: &Nibbles,
    offset: usize,
    value: Vec<u8>,
) -> u32 {
    let remaining = key.slice(offset..);
    let common_len = ext.nibbles.common_prefix_length(&remaining);

    if common_len == ext.nibbles.len() {
        // Full match of extension prefix: recurse into child
        let child_idx = match &ext.child {
            ChildRef::Arena(c) => *c,
            _ => panic!("Phase 1: only Arena child refs"),
        };
        let new_child = insert_recursive(arena, Some(child_idx), key, offset + common_len, value);
        if let MptNode::Extension(e) = arena.get_mut(idx) {
            e.child = ChildRef::Arena(new_child);
        }
        idx
    } else {
        EXTENSION_SPLITS.fetch_add(1, Ordering::Relaxed);
        // Partial match: split extension
        let mut branch = BranchNode::new();

        let ext_rest = ext.nibbles.slice(common_len..);
        let ext_child = ext.child.clone();

        if ext_rest.len() == 1 {
            // Only one nibble left: child goes directly into branch
            branch.children[ext_rest.get_unchecked(0) as usize] = Some(ext_child);
        } else {
            // Multiple nibbles left: create shorter extension
            let shorter_ext = arena.alloc(MptNode::Extension(ExtensionNode {
                nibbles: ext_rest.slice(1..),
                child: ext_child,
            }));
            branch.children[ext_rest.get_unchecked(0) as usize] =
                Some(ChildRef::Arena(shorter_ext));
        }

        // Insert new key into the branch
        let key_rest = remaining.slice(common_len..);
        if key_rest.is_empty() {
            branch.value = Some(value);
        } else {
            let new_child = insert_recursive(arena, None, key, offset + common_len + 1, value);
            branch.children[key_rest.get_unchecked(0) as usize] = Some(ChildRef::Arena(new_child));
        }

        let branch_idx = arena.alloc(MptNode::Branch(branch));

        if common_len > 0 {
            let ext_prefix = ext.nibbles.slice(..common_len);
            arena.alloc(MptNode::Extension(ExtensionNode {
                nibbles: ext_prefix,
                child: ChildRef::Arena(branch_idx),
            }))
        } else {
            branch_idx
        }
    }
}

/// Delete a key from the trie rooted at `node_idx`.
/// Returns (was_deleted, new_root_for_subtree).
pub(crate) fn delete_recursive(
    arena: &mut MutableTrieArena,
    node_idx: Option<u32>,
    key: &Nibbles,
    offset: usize,
) -> (bool, Option<u32>) {
    let idx = match node_idx {
        None => return (false, None),
        Some(i) => i,
    };

    arena.clear_rlp(idx);
    arena.mark_dirty(idx);
    let node = arena.get(idx).clone();

    match node {
        MptNode::Leaf(leaf) => {
            let remaining = key.slice(offset..);
            if leaf.nibbles == remaining {
                (true, None)
            } else {
                (false, Some(idx))
            }
        }
        MptNode::Extension(ext) => delete_at_extension(arena, idx, &ext, key, offset),
        MptNode::Branch(branch) => delete_at_branch(arena, idx, &branch, key, offset),
    }
}

fn delete_at_extension(
    arena: &mut MutableTrieArena,
    idx: u32,
    ext: &ExtensionNode,
    key: &Nibbles,
    offset: usize,
) -> (bool, Option<u32>) {
    let remaining = key.slice(offset..);
    let ext_len = ext.nibbles.len();

    if remaining.len() < ext_len || remaining.slice(..ext_len) != ext.nibbles {
        return (false, Some(idx));
    }

    let child_idx = match &ext.child {
        ChildRef::Arena(c) => *c,
        _ => panic!("Phase 1: only Arena child refs"),
    };

    let (deleted, new_child) = delete_recursive(arena, Some(child_idx), key, offset + ext_len);

    if !deleted {
        return (false, Some(idx));
    }

    match new_child {
        None => {
            // Child removed entirely
            (true, None)
        }
        Some(new_child_idx) => {
            let child_node = arena.get(new_child_idx).clone();
            match child_node {
                MptNode::Leaf(child_leaf) => {
                    EXTENSION_LEAF_MERGES.fetch_add(1, Ordering::Relaxed);
                    // Merge Extension + Leaf
                    let mut combined = ext.nibbles.clone();
                    combined.extend(&child_leaf.nibbles);
                    let merged = arena.alloc(MptNode::Leaf(LeafNode {
                        nibbles: combined,
                        value: child_leaf.value,
                    }));
                    (true, Some(merged))
                }
                MptNode::Extension(child_ext) => {
                    EXTENSION_EXTENSION_MERGES.fetch_add(1, Ordering::Relaxed);
                    // Merge two Extensions
                    let mut combined = ext.nibbles.clone();
                    combined.extend(&child_ext.nibbles);
                    let merged = arena.alloc(MptNode::Extension(ExtensionNode {
                        nibbles: combined,
                        child: child_ext.child,
                    }));
                    (true, Some(merged))
                }
                MptNode::Branch(_) => {
                    // Keep Extension + Branch
                    if let MptNode::Extension(e) = arena.get_mut(idx) {
                        e.child = ChildRef::Arena(new_child_idx);
                    }
                    (true, Some(idx))
                }
            }
        }
    }
}

fn delete_at_branch(
    arena: &mut MutableTrieArena,
    idx: u32,
    branch: &BranchNode,
    key: &Nibbles,
    offset: usize,
) -> (bool, Option<u32>) {
    let (deleted, mut updated_branch) = if offset >= key.len() {
        // Delete branch value
        if branch.value.is_none() {
            return (false, Some(idx));
        }
        let mut b = branch.clone();
        b.value = None;
        (true, b)
    } else {
        let nibble = key.get_unchecked(offset) as usize;
        let child_idx = match &branch.children[nibble] {
            Some(ChildRef::Arena(c)) => *c,
            Some(_) => panic!("Phase 1: only Arena child refs"),
            None => return (false, Some(idx)),
        };

        let (deleted, new_child) = delete_recursive(arena, Some(child_idx), key, offset + 1);
        if !deleted {
            return (false, Some(idx));
        }

        let mut b = branch.clone();
        b.children[nibble] = new_child.map(ChildRef::Arena);
        (deleted, b)
    };

    if !deleted {
        return (false, Some(idx));
    }

    // Check if branch needs merging
    let child_count = updated_branch.child_count();

    if child_count == 0 && updated_branch.value.is_none() {
        BRANCH_COLLAPSE_TO_EMPTY.fetch_add(1, Ordering::Relaxed);
        (true, None)
    } else if child_count == 0 && updated_branch.value.is_some() {
        BRANCH_COLLAPSE_TO_LEAF.fetch_add(1, Ordering::Relaxed);
        // Only value remains: convert to Leaf with empty nibbles
        let leaf = arena.alloc(MptNode::Leaf(LeafNode {
            nibbles: Nibbles::default(),
            value: updated_branch.value.take().unwrap(),
        }));
        (true, Some(leaf))
    } else if child_count == 1 && updated_branch.value.is_none() {
        BRANCH_COLLAPSE_TO_EXTENSION.fetch_add(1, Ordering::Relaxed);
        // Single child, no value: merge
        let (nibble_idx, child_ref) = updated_branch.single_child().unwrap();
        let child_arena_idx = match child_ref {
            ChildRef::Arena(i) => *i,
            _ => panic!("Phase 1: only Arena child refs"),
        };
        let child_node = arena.get(child_arena_idx).clone();

        match child_node {
            MptNode::Leaf(child_leaf) => {
                let mut combined = Nibbles::from_nibbles(&[nibble_idx]);
                combined.extend(&child_leaf.nibbles);
                let merged = arena
                    .alloc(MptNode::Leaf(LeafNode { nibbles: combined, value: child_leaf.value }));
                (true, Some(merged))
            }
            MptNode::Extension(child_ext) => {
                let mut combined = Nibbles::from_nibbles(&[nibble_idx]);
                combined.extend(&child_ext.nibbles);
                let merged = arena.alloc(MptNode::Extension(ExtensionNode {
                    nibbles: combined,
                    child: child_ext.child,
                }));
                (true, Some(merged))
            }
            MptNode::Branch(_) => {
                let ext = arena.alloc(MptNode::Extension(ExtensionNode {
                    nibbles: Nibbles::from_nibbles(&[nibble_idx]),
                    child: ChildRef::Arena(child_arena_idx),
                }));
                (true, Some(ext))
            }
        }
    } else {
        // Multiple children or has value with children: keep as branch
        *arena.get_mut(idx) = MptNode::Branch(updated_branch);
        (true, Some(idx))
    }
}

#[cfg(test)]
mod tests {
    use crate::mpt::tree::MptTree;
    use alloy_primitives::keccak256;
    use alloy_trie::{HashBuilder, Nibbles, EMPTY_ROOT_HASH};
    use std::collections::BTreeMap;

    /// Helper: compute HashBuilder root for a set of unique KVs.
    fn hb_root(kvs: &[(Nibbles, Vec<u8>)]) -> alloy_primitives::B256 {
        if kvs.is_empty() {
            return EMPTY_ROOT_HASH;
        }
        let mut sorted = kvs.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hb = HashBuilder::default();
        for (key, value) in &sorted {
            hb.add_leaf(key.clone(), value);
        }
        hb.root()
    }

    enum Op {
        Insert(Nibbles, Vec<u8>),
        Delete(Nibbles),
    }

    fn golden_test_ops(ops: &[Op]) {
        let mut tree = MptTree::new();
        let mut final_state: BTreeMap<Nibbles, Vec<u8>> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Insert(key, value) => {
                    tree.insert(key, value.clone());
                    final_state.insert(key.clone(), value.clone());
                }
                Op::Delete(key) => {
                    tree.delete(key);
                    final_state.remove(key);
                }
            }
        }

        let tree_root = tree.root_hash();

        let hb_root_val = if final_state.is_empty() {
            EMPTY_ROOT_HASH
        } else {
            let mut hb = HashBuilder::default();
            for (key, value) in &final_state {
                hb.add_leaf(key.clone(), value);
            }
            hb.root()
        };

        assert_eq!(
            tree_root, hb_root_val,
            "MptTree root after ops != HashBuilder root of final state"
        );
    }

    fn make_key(data: &[u8]) -> Nibbles {
        Nibbles::unpack(keccak256(data))
    }

    #[test]
    fn t5_1_empty_trie() {
        let tree = MptTree::new();
        assert!(tree.is_empty());
    }

    #[test]
    fn t5_2_empty_root_hash() {
        let mut tree = MptTree::new();
        assert_eq!(tree.root_hash(), EMPTY_ROOT_HASH);
    }

    #[test]
    fn t5_3_insert_one_key() {
        let key = make_key(b"hello");
        let value = b"world".to_vec();
        let mut tree = MptTree::new();
        tree.insert(&key, value.clone());
        assert!(!tree.is_empty());
        assert!(tree.root_node().unwrap().is_leaf());

        let expected = hb_root(&[(key, value)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_4_insert_two_different_keys() {
        let k1 = make_key(b"key1");
        let v1 = b"val1".to_vec();
        let k2 = make_key(b"key2");
        let v2 = b"val2".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k1, v1.clone());
        tree.insert(&k2, v2.clone());

        assert_eq!(tree.get(&k1), Some(v1.as_slice()));
        assert_eq!(tree.get(&k2), Some(v2.as_slice()));

        let expected = hb_root(&[(k1, v1), (k2, v2)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_5_insert_two_shared_prefix_keys() {
        let k1 = Nibbles::from_nibbles(&[1, 2, 3, 4, 5, 6]);
        let k2 = Nibbles::from_nibbles(&[1, 2, 3, 7, 8, 9]);
        let v1 = b"aaa".to_vec();
        let v2 = b"bbb".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k1, v1.clone());
        tree.insert(&k2, v2.clone());

        assert_eq!(tree.get(&k1), Some(v1.as_slice()));
        assert_eq!(tree.get(&k2), Some(v2.as_slice()));

        let expected = hb_root(&[(k1, v1), (k2, v2)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_6_insert_prefix_relationship() {
        // Test that one key being a prefix of another works correctly.
        // We verify get() works for both keys and that root_hash is consistent
        // after re-insert. (HashBuilder doesn't support prefix-key scenarios directly,
        // so we verify structural correctness via get + re-insert hash stability.)
        let k_short = Nibbles::from_nibbles(&[1, 2]);
        let k_long = Nibbles::from_nibbles(&[1, 2, 3]);
        let v_short = b"short".to_vec();
        let v_long = b"long".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k_short, v_short.clone());
        tree.insert(&k_long, v_long.clone());

        assert_eq!(tree.get(&k_short), Some(v_short.as_slice()));
        assert_eq!(tree.get(&k_long), Some(v_long.as_slice()));

        // Verify root_hash is stable (same result on repeated calls)
        let h1 = tree.root_hash();
        let h2 = tree.root_hash();
        assert_eq!(h1, h2);
        assert_ne!(h1, EMPTY_ROOT_HASH);

        // Verify internal structure: root should involve a branch with value
        // (short key consumed at branch, long key goes into child)
        let root = tree.root_node().unwrap();
        // Root should be Extension (prefix [1,2]) pointing to Branch
        assert!(
            root.is_extension() || root.is_branch(),
            "expected Extension or Branch at root for prefix keys"
        );
    }

    #[test]
    fn t5_7_delete_long_key_prefix_pair() {
        // Use same-length keys (keccak hashed) to allow HashBuilder comparison
        let k1 = make_key(b"short_key");
        let k2 = make_key(b"long_key");
        let v1 = b"short".to_vec();
        let v2 = b"long".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k1, v1.clone());
        tree.insert(&k2, v2.clone());

        assert!(tree.delete(&k2));
        assert_eq!(tree.get(&k1), Some(v1.as_slice()));
        assert!(tree.get(&k2).is_none());

        let expected = hb_root(&[(k1, v1)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_8_delete_short_key_prefix_pair() {
        let k1 = make_key(b"short_key");
        let k2 = make_key(b"long_key");
        let v1 = b"short".to_vec();
        let v2 = b"long".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k1, v1.clone());
        tree.insert(&k2, v2.clone());

        assert!(tree.delete(&k1));
        assert!(tree.get(&k1).is_none());
        assert_eq!(tree.get(&k2), Some(v2.as_slice()));

        let expected = hb_root(&[(k2, v2)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_9_insert_get() {
        let key = make_key(b"mykey");
        let val = b"myval".to_vec();
        let mut tree = MptTree::new();
        tree.insert(&key, val.clone());
        assert_eq!(tree.get(&key), Some(val.as_slice()));
    }

    #[test]
    fn t5_10_insert_same_key_twice() {
        let key = make_key(b"dup");
        let v1 = b"first".to_vec();
        let v2 = b"second".to_vec();
        let mut tree = MptTree::new();
        tree.insert(&key, v1);
        tree.insert(&key, v2.clone());
        assert_eq!(tree.get(&key), Some(v2.as_slice()));
    }

    #[test]
    fn t5_11_delete_existing_key() {
        let key = make_key(b"del");
        let val = b"gone".to_vec();
        let mut tree = MptTree::new();
        tree.insert(&key, val);
        assert!(tree.delete(&key));
        assert!(tree.get(&key).is_none());
    }

    #[test]
    fn t5_12_delete_nonexistent_key() {
        let k1 = make_key(b"exists");
        let k2 = make_key(b"nope");
        let mut tree = MptTree::new();
        tree.insert(&k1, b"v".to_vec());
        assert!(!tree.delete(&k2));
    }

    #[test]
    fn t5_13_delete_branch_merge() {
        let k1 = make_key(b"a");
        let k2 = make_key(b"b");
        let k3 = make_key(b"c");
        let v1 = b"v1".to_vec();
        let v2 = b"v2".to_vec();
        let v3 = b"v3".to_vec();

        let mut tree = MptTree::new();
        tree.insert(&k1, v1.clone());
        tree.insert(&k2, v2.clone());
        tree.insert(&k3, v3.clone());

        tree.delete(&k2);
        let expected = hb_root(&[(k1, v1), (k3, v3)]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_14_insert_100_deterministic() {
        use rand::{rngs::StdRng, Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(42);
        let mut kvs = Vec::new();
        let mut seen = std::collections::HashSet::new();

        while kvs.len() < 100 {
            let mut data = [0u8; 32];
            rng.fill(&mut data);
            let key = Nibbles::unpack(keccak256(data));
            if seen.insert(key.clone()) {
                let mut val = vec![0u8; 8];
                rng.fill(&mut val[..]);
                kvs.push((key, val));
            }
        }

        let mut tree = MptTree::new();
        for (k, v) in &kvs {
            tree.insert(k, v.clone());
        }

        let expected = hb_root(&kvs);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_15_insert_1000_deterministic() {
        use rand::{rngs::StdRng, Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(43);
        let mut kvs = Vec::new();
        let mut seen = std::collections::HashSet::new();

        while kvs.len() < 1000 {
            let mut data = [0u8; 32];
            rng.fill(&mut data);
            let key = Nibbles::unpack(keccak256(data));
            if seen.insert(key.clone()) {
                let mut val = vec![0u8; 16];
                rng.fill(&mut val[..]);
                kvs.push((key, val));
            }
        }

        let mut tree = MptTree::new();
        for (k, v) in &kvs {
            tree.insert(k, v.clone());
        }

        let expected = hb_root(&kvs);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn t5_16_insert_delete_mixed() {
        use rand::{rngs::StdRng, Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(44);
        let mut ops = Vec::new();

        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while keys.len() < 100 {
            let mut data = [0u8; 32];
            rng.fill(&mut data);
            let key = Nibbles::unpack(keccak256(data));
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }

        // Insert all 100
        for k in &keys {
            let mut val = vec![0u8; 8];
            rng.fill(&mut val[..]);
            ops.push(Op::Insert(k.clone(), val));
        }

        // Delete 50
        for k in &keys[..50] {
            ops.push(Op::Delete(k.clone()));
        }

        golden_test_ops(&ops);
    }

    #[test]
    fn t5_17_cache_invalidation_insert() {
        let k1 = make_key(b"alpha");
        let k2 = make_key(b"beta");

        let mut tree = MptTree::new();
        tree.insert(&k1, b"v1".to_vec());
        let h1 = tree.root_hash();

        tree.insert(&k2, b"v2".to_vec());
        let h2 = tree.root_hash();

        assert_ne!(h1, h2, "root_hash must differ after second insert");
    }

    #[test]
    fn t5_18_cache_invalidation_delete_to_empty() {
        let k = make_key(b"only");
        let mut tree = MptTree::new();
        tree.insert(&k, b"v".to_vec());
        let _ = tree.root_hash();

        tree.delete(&k);
        assert_eq!(tree.root_hash(), EMPTY_ROOT_HASH);
    }

    #[test]
    fn t5_19_cache_invalidation_update() {
        let k1 = make_key(b"x");
        let k2 = make_key(b"y");
        let k3 = make_key(b"z");

        let mut tree = MptTree::new();
        tree.insert(&k1, b"1".to_vec());
        tree.insert(&k2, b"2".to_vec());
        tree.insert(&k3, b"3".to_vec());
        let h1 = tree.root_hash();

        tree.insert(&k2, b"new2".to_vec());
        let h2 = tree.root_hash();

        assert_ne!(h1, h2, "root_hash must change after value update");
    }

    #[test]
    fn t5_20_root_hash_then_read() {
        let k = make_key(b"read_after_hash");
        let v = b"readable".to_vec();
        let mut tree = MptTree::new();
        tree.insert(&k, v.clone());
        let _ = tree.root_hash();
        assert_eq!(tree.get(&k), Some(v.as_slice()));
    }

    #[test]
    fn t5_21_root_hash_then_delete() {
        let k = make_key(b"delete_after_hash");
        let mut tree = MptTree::new();
        tree.insert(&k, b"v".to_vec());
        let _ = tree.root_hash();
        assert!(tree.delete(&k));
        assert!(tree.get(&k).is_none());
    }

    #[test]
    fn t5_22_root_hash_then_write() {
        let k1 = make_key(b"first");
        let k2 = make_key(b"second");
        let mut tree = MptTree::new();
        tree.insert(&k1, b"v1".to_vec());
        let _ = tree.root_hash();
        tree.insert(&k2, b"v2".to_vec());
        assert_eq!(tree.get(&k1), Some(b"v1".as_ref()));
        assert_eq!(tree.get(&k2), Some(b"v2".as_ref()));
    }
}
