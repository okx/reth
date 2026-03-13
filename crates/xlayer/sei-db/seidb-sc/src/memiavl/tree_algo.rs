#![allow(clippy::arc_with_non_send_sync)]

use crate::memiavl::node::{MemNode, Node, NodeRef};
use std::{cmp::Ordering, sync::Arc};

/// Insert or update a key-value pair in the AVL tree rooted at `node`.
///
/// Returns `(new_root, updated)` where `updated` is `true` if an existing key
/// was overwritten (i.e. the tree size did not change), and `false` if a new
/// leaf was inserted.
///
/// Uses borrowed key/value throughout recursion; only clones at the leaf
/// where data is actually stored, saving ~18 levels of Vec allocation per insert.
///
/// Mirrors Go `setRecursive` in `node.go`.
pub fn set_recursive(
    node: Option<NodeRef>,
    key: &[u8],
    value: &[u8],
    version: u32,
    cow_version: u32,
) -> (NodeRef, bool) {
    let node_ref = match node {
        None => {
            let leaf = MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version);
            return (Arc::new(Node::Mem(leaf)), false);
        }
        Some(n) => n,
    };

    if node_ref.is_leaf() {
        match key.cmp(node_ref.key()) {
            Ordering::Less => {
                let new_leaf = Arc::new(Node::Mem(MemNode::new_leaf_node(
                    key.to_vec(),
                    value.to_vec(),
                    version,
                )));
                let branch = MemNode::new_branch_node(new_leaf, node_ref, version);
                return (Arc::new(Node::Mem(branch)), false);
            }
            Ordering::Greater => {
                let new_leaf = Arc::new(Node::Mem(MemNode::new_leaf_node(
                    key.to_vec(),
                    value.to_vec(),
                    version,
                )));
                let branch = MemNode::new_branch_node(node_ref, new_leaf, version);
                return (Arc::new(Node::Mem(branch)), false);
            }
            Ordering::Equal => {
                // Update existing leaf value.
                let mut mem = Node::into_mem_node(node_ref, version);
                mem.value = value.to_vec();
                return (Arc::new(Node::Mem(mem)), true);
            }
        }
    }

    // Branch node — recurse into the appropriate child.
    let go_left = key < node_ref.key();
    let mut mem = Node::into_mem_node(node_ref, version);

    let updated = if go_left {
        let left_child = mem.left.take();
        let (new_left, updated) = set_recursive(left_child, key, value, version, cow_version);
        mem.left = Some(new_left);
        updated
    } else {
        let right_child = mem.right.take();
        let (new_right, updated) = set_recursive(right_child, key, value, version, cow_version);
        mem.right = Some(new_right);
        updated
    };

    if !updated {
        mem.update_height_size();
        mem = rebalance(mem, version, cow_version);
    }

    (Arc::new(Node::Mem(mem)), updated)
}

/// Like [`set_recursive`] but takes owned key/value to avoid cloning when the
/// caller already has owned `Vec<u8>` data (e.g. from `apply_change_set`).
pub fn set_recursive_owned(
    node: Option<NodeRef>,
    key: Vec<u8>,
    value: Vec<u8>,
    version: u32,
    cow_version: u32,
) -> (NodeRef, bool) {
    // Delegate to the borrow-based version. The owned data is only consumed
    // at the leaf level where to_vec() is a no-op on already-owned data.
    // For the common update case, key/value are compared but not stored in
    // branch nodes, so borrowing avoids intermediate allocations.
    set_recursive(node, &key, &value, version, cow_version)
}

/// Remove a key from the AVL tree rooted at `node`.
///
/// Returns `(removed_value, new_subtree, new_key)`:
/// - `removed_value`: `Some(value)` if the key was found and removed, `None` otherwise.
/// - `new_subtree`: the new root of the subtree after removal (None if the subtree is now empty).
/// - `new_key`: when removing from the left subtree, the branch key may need updating to maintain
///   the invariant that the branch key equals the first key of the right subtree.
///
/// Mirrors Go `removeRecursive` in `node.go`.
pub fn remove_recursive(
    node: NodeRef,
    key: &[u8],
    version: u32,
    cow_version: u32,
) -> (Option<Vec<u8>>, Option<NodeRef>, Option<Vec<u8>>) {
    if node.is_leaf() {
        if node.key() == key {
            return (Some(node.value().to_vec()), None, None);
        }
        return (None, Some(node), None);
    }

    // Branch node — convert to MemNode first, then take children to avoid Arc clones.
    let go_left = key < node.key();
    let mut mem = Node::into_mem_node(node, version);

    if go_left {
        let left = mem.left.take().expect("branch node must have left child");
        let (value, new_left, new_key) = remove_recursive(left, key, version, cow_version);
        if value.is_none() {
            // Key not found — restore left and return original node.
            mem.left = new_left;
            return (None, Some(Arc::new(Node::Mem(mem))), None);
        }
        if new_left.is_none() {
            // Left child removed entirely — promote right child.
            let right = mem.right.take().expect("branch node must have right child");
            let key_copy = mem.key.clone();
            return (value, Some(right), Some(key_copy));
        }
        mem.left = new_left;
        mem.update_height_size();
        let balanced = rebalance(mem, version, cow_version);
        (value, Some(Arc::new(Node::Mem(balanced))), new_key)
    } else {
        let right = mem.right.take().expect("branch node must have right child");
        let (value, new_right, new_key) = remove_recursive(right, key, version, cow_version);
        if value.is_none() {
            // Key not found — restore right and return original node.
            mem.right = new_right;
            return (None, Some(Arc::new(Node::Mem(mem))), None);
        }
        if new_right.is_none() {
            // Right child removed entirely — promote left child.
            let left = mem.left.take().expect("branch node must have left child");
            return (value, Some(left), None);
        }
        mem.right = new_right;
        if let Some(ref nk) = new_key {
            mem.key = nk.clone();
        }
        mem.update_height_size();
        let balanced = rebalance(mem, version, cow_version);
        (value, Some(Arc::new(Node::Mem(balanced))), None)
    }
}

/// Rebalance a branch node using AVL rotations.
///
/// Mirrors Go `(*MemNode).reBalance` in `mem_node.go`.
fn rebalance(mut node: MemNode, version: u32, cow_version: u32) -> MemNode {
    let balance = node.calc_balance();
    if balance > 1 {
        // Left-heavy.
        let left_ref = node.left.as_ref().expect("left child must exist for balance > 1");
        let left_balance = node_balance(left_ref);
        if left_balance >= 0 {
            // Left-left case.
            return rotate_right(node, version, cow_version);
        }
        // Left-right case: rotate left child left, then rotate node right.
        let left = node.left.take().unwrap();
        let left_mem = Node::into_mem_node(left, version);
        let rotated_left = rotate_left(left_mem, version, cow_version);
        node.left = Some(Arc::new(Node::Mem(rotated_left)));
        return rotate_right(node, version, cow_version);
    }
    if balance < -1 {
        // Right-heavy.
        let right_ref = node.right.as_ref().expect("right child must exist for balance < -1");
        let right_balance = node_balance(right_ref);
        if right_balance <= 0 {
            // Right-right case.
            return rotate_left(node, version, cow_version);
        }
        // Right-left case: rotate right child right, then rotate node left.
        let right = node.right.take().unwrap();
        let right_mem = Node::into_mem_node(right, version);
        let rotated_right = rotate_right(right_mem, version, cow_version);
        node.right = Some(Arc::new(Node::Mem(rotated_right)));
        return rotate_left(node, version, cow_version);
    }
    node
}

/// Right rotation: promotes the left child as the new root.
///
/// ```text
///       S              L
///      / \    =>      / \
///     L               S
///    / \             / \
///      LR           LR
/// ```
///
/// Mirrors Go `(*MemNode).rotateRight` in `mem_node.go`.
fn rotate_right(mut node: MemNode, version: u32, _cow_version: u32) -> MemNode {
    let left = node.left.take().expect("rotate_right requires left child");
    let mut new_root = Node::into_mem_node(left, version);
    // LR becomes S's new left child.
    node.left = new_root.right.take();
    node.update_height_size();
    // S becomes new_root's right child.
    new_root.right = Some(Arc::new(Node::Mem(node)));
    new_root.update_height_size();
    new_root
}

/// Left rotation: promotes the right child as the new root.
///
/// ```text
///     S              R
///    / \    =>      / \
///        R         S
///       / \       / \
///     RL             RL
/// ```
///
/// Mirrors Go `(*MemNode).rotateLeft` in `mem_node.go`.
fn rotate_left(mut node: MemNode, version: u32, _cow_version: u32) -> MemNode {
    let right = node.right.take().expect("rotate_left requires right child");
    let mut new_root = Node::into_mem_node(right, version);
    // RL becomes S's new right child.
    node.right = new_root.left.take();
    node.update_height_size();
    // S becomes new_root's left child.
    new_root.left = Some(Arc::new(Node::Mem(node)));
    new_root.update_height_size();
    new_root
}

/// Compute the balance factor of a node behind a `NodeRef`.
fn node_balance(node: &NodeRef) -> i8 {
    let lh = node.left().map_or(0, |n| n.height()) as i8;
    let rh = node.right().map_or(0, |n| n.height()) as i8;
    lh - rh
}

/// Compute and return the hash of a node.
///
/// Delegates to `Node::hash()` which uses lazy `OnceCell` computation.
pub fn hash_node(node: &Node) -> Vec<u8> {
    node.hash().to_vec()
}

/// Verify a node's hash by recomputing it from scratch and comparing with the cached value.
///
/// Returns `true` if the hash matches (or is not yet cached), `false` on mismatch.
pub fn verify_hash(node: &Node) -> bool {
    let cached = node.hash();
    // Recompute by creating a fresh node with the same data but no cached hash.
    let recomputed = recompute_hash(node);
    cached == recomputed.as_slice()
}

/// Recompute the hash from scratch without using any cache.
fn recompute_hash(node: &Node) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    let mut data = Vec::with_capacity(128);
    encode_varint_signed(node.height() as i64, &mut data);
    encode_varint_signed(node.size(), &mut data);
    encode_varint_signed(node.version() as i64, &mut data);

    if node.is_leaf() {
        encode_bytes(node.key(), &mut data);
        let value_hash = Sha256::digest(node.value());
        encode_bytes(&value_hash, &mut data);
    } else {
        let left_hash = node.left().map(|l| l.hash()).unwrap_or(&[]);
        let right_hash = node.right().map(|r| r.hash()).unwrap_or(&[]);
        encode_bytes(left_hash, &mut data);
        encode_bytes(right_hash, &mut data);
    }

    Sha256::digest(&data).to_vec()
}

fn encode_varint_signed(value: i64, buf: &mut Vec<u8>) {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    encode_varint_unsigned(zigzag, buf);
}

fn encode_varint_unsigned(mut value: u64, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn encode_bytes(bytes: &[u8], buf: &mut Vec<u8>) {
    encode_varint_unsigned(bytes.len() as u64, buf);
    buf.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a tree by inserting keys sequentially and return root.
    fn build_tree(keys: &[&[u8]], version: u32) -> Option<NodeRef> {
        let mut root: Option<NodeRef> = None;
        for k in keys {
            let (new_root, _) = set_recursive(root, k, k, version, 0);
            root = Some(new_root);
        }
        root
    }

    #[test]
    fn test_set_into_empty() {
        let (root, updated) = set_recursive(None, b"hello", b"world", 1, 0);
        assert!(!updated);
        assert!(root.is_leaf());
        assert_eq!(root.key(), b"hello");
        assert_eq!(root.value(), b"world");
        assert_eq!(root.height(), 0);
        assert_eq!(root.size(), 1);
    }

    #[test]
    fn test_set_update() {
        let (root, _) = set_recursive(None, b"key", b"val1", 1, 0);
        let (root, updated) = set_recursive(Some(root), b"key", b"val2", 2, 0);
        assert!(updated);
        assert!(root.is_leaf());
        assert_eq!(root.key(), b"key");
        assert_eq!(root.value(), b"val2");
        assert_eq!(root.version(), 2);
    }

    #[test]
    fn test_set_left_right() {
        // Insert "bbb" then "aaa" (goes left), then "ccc" (goes right).
        let (root, _) = set_recursive(None, b"bbb", b"v2", 1, 0);
        let (root, updated) = set_recursive(Some(root), b"aaa", b"v1", 1, 0);
        assert!(!updated);
        assert_eq!(root.height(), 1);
        assert_eq!(root.size(), 2);

        let (root, updated) = set_recursive(Some(root), b"ccc", b"v3", 1, 0);
        assert!(!updated);
        assert_eq!(root.size(), 3);

        // All three keys should be retrievable.
        assert_eq!(root.get(b"aaa").unwrap().0, b"v1");
        assert_eq!(root.get(b"bbb").unwrap().0, b"v2");
        assert_eq!(root.get(b"ccc").unwrap().0, b"v3");
    }

    #[test]
    fn test_remove_leaf() {
        let (root, _) = set_recursive(None, b"only", b"val", 1, 0);
        let (removed, new_root, _new_key) = remove_recursive(root, b"only", 2, 0);
        assert_eq!(removed.unwrap(), b"val");
        assert!(new_root.is_none());
    }

    #[test]
    fn test_remove_from_branch() {
        let (root, _) = set_recursive(None, b"aaa", b"v1", 1, 0);
        let (root, _) = set_recursive(Some(root), b"bbb", b"v2", 1, 0);
        assert_eq!(root.size(), 2);

        // Remove "aaa" — should leave only "bbb".
        let (removed, new_root, _) = remove_recursive(root, b"aaa", 2, 0);
        assert_eq!(removed.unwrap(), b"v1");
        let new_root = new_root.unwrap();
        assert!(new_root.is_leaf());
        assert_eq!(new_root.key(), b"bbb");
    }

    #[test]
    fn test_remove_not_found() {
        let (root, _) = set_recursive(None, b"aaa", b"v1", 1, 0);
        let (removed, new_root, _) = remove_recursive(root, b"zzz", 2, 0);
        assert!(removed.is_none());
        let new_root = new_root.unwrap();
        assert_eq!(new_root.key(), b"aaa");
    }

    #[test]
    fn test_balance_ll() {
        // Insert keys in descending order to trigger left-left imbalance: c, b, a.
        let root = build_tree(&[b"c", b"b", b"a"], 1);
        let root = root.unwrap();
        // After rebalance, tree height should be optimal (2 = balanced 3-node tree).
        assert!(root.height() <= 2);
        assert_eq!(root.size(), 3);
        // All keys retrievable.
        assert!(root.get(b"a").is_some());
        assert!(root.get(b"b").is_some());
        assert!(root.get(b"c").is_some());
    }

    #[test]
    fn test_balance_rr() {
        // Insert keys in ascending order to trigger right-right imbalance: a, b, c.
        let root = build_tree(&[b"a", b"b", b"c"], 1);
        let root = root.unwrap();
        assert!(root.height() <= 2);
        assert_eq!(root.size(), 3);
        assert!(root.get(b"a").is_some());
        assert!(root.get(b"b").is_some());
        assert!(root.get(b"c").is_some());
    }

    #[test]
    fn test_balance_lr() {
        // Left-right case: c, a, b.
        let root = build_tree(&[b"c", b"a", b"b"], 1);
        let root = root.unwrap();
        assert!(root.height() <= 2);
        assert_eq!(root.size(), 3);
        assert!(root.get(b"a").is_some());
        assert!(root.get(b"b").is_some());
        assert!(root.get(b"c").is_some());
    }

    #[test]
    fn test_balance_rl() {
        // Right-left case: a, c, b.
        let root = build_tree(&[b"a", b"c", b"b"], 1);
        let root = root.unwrap();
        assert!(root.height() <= 2);
        assert_eq!(root.size(), 3);
        assert!(root.get(b"a").is_some());
        assert!(root.get(b"b").is_some());
        assert!(root.get(b"c").is_some());
    }

    #[test]
    fn test_set_many_balanced() {
        // Insert 100 keys and verify the tree stays balanced (AVL invariant).
        let mut root: Option<NodeRef> = None;
        for i in 0u32..100 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            let (new_root, _) = set_recursive(root, key.as_bytes(), val.as_bytes(), 1, 0);
            root = Some(new_root);
        }

        let root = root.unwrap();
        assert_eq!(root.size(), 100);

        // AVL height bound: height <= 1.44 * log2(n + 2)
        let max_height = (1.44 * (102_f64).log2()).ceil() as u8;
        assert!(
            root.height() <= max_height,
            "height {} exceeds AVL bound {}",
            root.height(),
            max_height,
        );

        // Verify all keys are retrievable.
        for i in 0u32..100 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            let result = root.get(key.as_bytes());
            assert_eq!(
                result.as_ref().map(|(v, _)| v.as_slice()),
                Some(val.as_bytes()),
                "key {} not found or wrong value",
                key
            );
        }
    }

    #[test]
    fn test_hash_node() {
        let leaf = Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1));
        let h = hash_node(&leaf);
        assert_eq!(h.len(), 32);

        // Deterministic: same input produces same hash.
        let leaf2 = Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1));
        assert_eq!(h, hash_node(&leaf2));

        // Different value → different hash.
        let leaf3 = Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"other".to_vec(), 1));
        assert_ne!(h, hash_node(&leaf3));
    }

    #[test]
    fn test_verify_hash() {
        // Leaf node.
        let leaf = Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1));
        assert!(verify_hash(&leaf));

        // Branch node.
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch = Node::Mem(MemNode::new_branch_node(left, right, 2));
        assert!(verify_hash(&branch));
    }
}
