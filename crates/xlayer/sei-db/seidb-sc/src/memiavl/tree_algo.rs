#![allow(clippy::arc_with_non_send_sync)]

use crate::memiavl::{
    arena::{cow_to_mutable, resolve_mem_node, FrozenArena, MutableArena, NodeIdx},
    node::{persisted_to_mem_arena, MemNode, Node, NodeRef},
    snapshot::Snapshot,
};
use std::{cmp::Ordering, sync::Arc};

/// Insert or update a key-value pair in the AVL tree rooted at `node`.
///
/// Returns `(new_root, updated)` where `updated` is `true` if an existing key
/// was overwritten (i.e. the tree size did not change), and `false` if a new
/// leaf was inserted.
///
/// Uses borrowed key/value throughout recursion; only clones at the leaf
/// where data is actually stored.
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

// ===========================================================================
// Arena-based algorithms (hot path — no Arc allocation)
// ===========================================================================

/// Read metadata (height, size, key) from a node at the given index.
///
/// Works for any generation: mutable, frozen, or persisted.
/// Returns (height, size, key_clone).
fn read_node_meta(
    arena: &MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
) -> (u8, i64, Vec<u8>) {
    if idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required for persisted node");
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        (pn.height(), pn.size(), pn.key().to_vec())
    } else {
        let node = resolve_mem_node(arena, frozen, current_gen, idx);
        (node.height, node.size, node.key.clone())
    }
}

/// Read just height and size from a node at the given index.
fn read_height_size(
    arena: &MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
) -> (u8, i64) {
    if idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required for persisted node");
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        (pn.height(), pn.size())
    } else {
        let node = resolve_mem_node(arena, frozen, current_gen, idx);
        (node.height, node.size)
    }
}

/// Materialize a persisted node into the mutable arena.
/// Returns the arena index of the new MemNode.
fn materialize_persisted(
    arena: &mut MutableArena,
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
    version: u32,
) -> u32 {
    let snap = snapshot.as_ref().expect("snapshot required for persisted node");
    let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
    let (mut mem, left_pn, right_pn) = persisted_to_mem_arena(&pn);
    mem.version = version;

    // Materialize children into arena if they exist
    if let Some(lpn) = left_pn {
        let left_is_leaf = lpn.is_leaf();
        let left_pn_idx = NodeIdx::persisted(lpn.index, left_is_leaf);
        // Don't materialize recursively — store as persisted NodeIdx
        mem.left_idx = Some(left_pn_idx);
    }
    if let Some(rpn) = right_pn {
        let right_is_leaf = rpn.is_leaf();
        let right_pn_idx = NodeIdx::persisted(rpn.index, right_is_leaf);
        mem.right_idx = Some(right_pn_idx);
    }

    arena.alloc(mem)
}

/// CoW-materialize a node (any generation) into the mutable arena.
/// Handles persisted, frozen, and mutable nodes.
fn cow_any_to_mutable(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
    version: u32,
    cow_version: u32,
) -> u32 {
    if idx.is_persisted() {
        materialize_persisted(arena, snapshot, current_gen, idx, version)
    } else {
        cow_to_mutable(arena, current_gen, frozen, idx, version, cow_version)
    }
}

/// Insert or update a key-value pair using arena-based node storage.
///
/// Returns `(new_root_idx, updated)`.
pub fn set_recursive_arena(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    node: Option<NodeIdx>,
    key: &[u8],
    value: &[u8],
    version: u32,
    cow_version: u32,
) -> (NodeIdx, bool) {
    let idx = match node {
        None => {
            let leaf = MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version);
            let slot = arena.alloc(leaf);
            return (NodeIdx::mem(current_gen, slot), false);
        }
        Some(i) => i,
    };

    // Read metadata before any mutation
    let (height, _size, node_key) = read_node_meta(arena, frozen, snapshot, current_gen, idx);

    if height == 0 {
        // Leaf node
        match key.cmp(node_key.as_slice()) {
            Ordering::Less => {
                let new_leaf = MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version);
                let new_leaf_slot = arena.alloc(new_leaf);
                let new_leaf_idx = NodeIdx::mem(current_gen, new_leaf_slot);

                // Read existing leaf's metadata for branch construction
                let (eh, es, _ek) = read_node_meta(arena, frozen, snapshot, current_gen, idx);
                let branch = MemNode::new_branch_node_idx(
                    new_leaf_idx,
                    idx,
                    0,
                    eh,
                    1,
                    es,
                    node_key,
                    version,
                );
                let branch_slot = arena.alloc(branch);
                return (NodeIdx::mem(current_gen, branch_slot), false);
            }
            Ordering::Greater => {
                let new_leaf = MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version);
                let new_leaf_slot = arena.alloc(new_leaf);
                let new_leaf_idx = NodeIdx::mem(current_gen, new_leaf_slot);

                // Read existing leaf's metadata for branch construction
                let (eh, es, _ek) = read_node_meta(arena, frozen, snapshot, current_gen, idx);
                // new_leaf is on the right, so right_key = key
                let branch = MemNode::new_branch_node_idx(
                    idx,
                    new_leaf_idx,
                    eh,
                    0,
                    es,
                    1,
                    key.to_vec(),
                    version,
                );
                let branch_slot = arena.alloc(branch);
                return (NodeIdx::mem(current_gen, branch_slot), false);
            }
            Ordering::Equal => {
                // Update existing leaf
                let slot = cow_any_to_mutable(
                    arena,
                    frozen,
                    snapshot,
                    current_gen,
                    idx,
                    version,
                    cow_version,
                );
                arena.get_mut(slot).value = value.to_vec();
                return (NodeIdx::mem(current_gen, slot), true);
            }
        }
    }

    // Branch node — CoW it, then recurse
    let go_left = key < node_key.as_slice();
    let slot = cow_any_to_mutable(arena, frozen, snapshot, current_gen, idx, version, cow_version);

    let updated = if go_left {
        let left_child = arena.get(slot).left_idx;
        let (new_left, updated) = set_recursive_arena(
            arena,
            frozen,
            snapshot,
            current_gen,
            left_child,
            key,
            value,
            version,
            cow_version,
        );
        arena.get_mut(slot).left_idx = Some(new_left);
        updated
    } else {
        let right_child = arena.get(slot).right_idx;
        let (new_right, updated) = set_recursive_arena(
            arena,
            frozen,
            snapshot,
            current_gen,
            right_child,
            key,
            value,
            version,
            cow_version,
        );
        arena.get_mut(slot).right_idx = Some(new_right);
        // Update key if right child changed (key = right's leftmost key)
        if !updated {
            // The key of this branch is the leftmost key of the right subtree.
            // After inserting into the right subtree, the key doesn't change
            // (insertion to right subtree doesn't change its leftmost key
            // unless we inserted at the very left, but then go_left would be true).
            // However, after rotation, keys might change. We'll handle that in rebalance.
        }
        updated
    };

    if !updated {
        // Update height and size from children
        let left_idx = arena.get(slot).left_idx;
        let right_idx = arena.get(slot).right_idx;
        let (lh, ls) =
            left_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        let (rh, rs) =
            right_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        arena.get_mut(slot).update_height_size_with(lh, rh, ls, rs);

        let result_idx =
            rebalance_arena(arena, frozen, snapshot, current_gen, slot, version, cow_version);
        return (NodeIdx::mem(current_gen, result_idx), false);
    }

    (NodeIdx::mem(current_gen, slot), true)
}

/// Remove a key from the AVL tree using arena-based storage.
///
/// Returns `(removed_value, new_subtree, new_key)`.
pub fn remove_recursive_arena(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    node: NodeIdx,
    key: &[u8],
    version: u32,
    cow_version: u32,
) -> (Option<Vec<u8>>, Option<NodeIdx>, Option<Vec<u8>>) {
    let (height, _size, node_key) = read_node_meta(arena, frozen, snapshot, current_gen, node);

    if height == 0 {
        // Leaf node
        if node_key == key {
            // Read value before removing
            let value = if node.is_persisted() {
                let snap = snapshot.as_ref().unwrap();
                let pn = snap.node_at(node.persisted_index(), node.persisted_is_leaf());
                pn.value().unwrap_or(&[]).to_vec()
            } else {
                let n = resolve_mem_node(arena, frozen, current_gen, node);
                n.value.clone()
            };
            return (Some(value), None, None);
        }
        return (None, Some(node), None);
    }

    // Branch node
    let go_left = key < node_key.as_slice();
    let slot = cow_any_to_mutable(arena, frozen, snapshot, current_gen, node, version, cow_version);

    if go_left {
        let left = arena.get(slot).left_idx.expect("branch must have left child");
        let (value, new_left, new_key) = remove_recursive_arena(
            arena,
            frozen,
            snapshot,
            current_gen,
            left,
            key,
            version,
            cow_version,
        );
        if value.is_none() {
            arena.get_mut(slot).left_idx = Some(left);
            return (None, Some(NodeIdx::mem(current_gen, slot)), None);
        }
        if new_left.is_none() {
            // Left removed entirely — promote right
            let right = arena.get(slot).right_idx.expect("branch must have right child");
            let key_copy = arena.get(slot).key.clone();
            return (value, Some(right), Some(key_copy));
        }
        arena.get_mut(slot).left_idx = new_left;
        // Update height/size
        let left_idx = arena.get(slot).left_idx;
        let right_idx = arena.get(slot).right_idx;
        let (lh, ls) =
            left_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        let (rh, rs) =
            right_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        arena.get_mut(slot).update_height_size_with(lh, rh, ls, rs);

        let balanced =
            rebalance_arena(arena, frozen, snapshot, current_gen, slot, version, cow_version);
        (value, Some(NodeIdx::mem(current_gen, balanced)), new_key)
    } else {
        let right = arena.get(slot).right_idx.expect("branch must have right child");
        let (value, new_right, new_key) = remove_recursive_arena(
            arena,
            frozen,
            snapshot,
            current_gen,
            right,
            key,
            version,
            cow_version,
        );
        if value.is_none() {
            arena.get_mut(slot).right_idx = Some(right);
            return (None, Some(NodeIdx::mem(current_gen, slot)), None);
        }
        if new_right.is_none() {
            // Right removed entirely — promote left
            let left = arena.get(slot).left_idx.expect("branch must have left child");
            return (value, Some(left), None);
        }
        arena.get_mut(slot).right_idx = new_right;
        if let Some(ref nk) = new_key {
            arena.get_mut(slot).key = nk.clone();
        }
        // Update height/size
        let left_idx = arena.get(slot).left_idx;
        let right_idx = arena.get(slot).right_idx;
        let (lh, ls) =
            left_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        let (rh, rs) =
            right_idx.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
        arena.get_mut(slot).update_height_size_with(lh, rh, ls, rs);

        let balanced =
            rebalance_arena(arena, frozen, snapshot, current_gen, slot, version, cow_version);
        (value, Some(NodeIdx::mem(current_gen, balanced)), None)
    }
}

/// Rebalance a node in the mutable arena. Returns the (possibly new) arena slot.
fn rebalance_arena(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    slot: u32,
    version: u32,
    cow_version: u32,
) -> u32 {
    let left_idx = arena.get(slot).left_idx;
    let right_idx = arena.get(slot).right_idx;
    let lh = left_idx.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
    let rh = right_idx.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
    let balance = MemNode::calc_balance_with(lh, rh);

    if balance > 1 {
        // Left-heavy
        let left = left_idx.expect("left must exist for balance > 1");
        let left_slot =
            cow_any_to_mutable(arena, frozen, snapshot, current_gen, left, version, cow_version);
        let ll = arena.get(left_slot).left_idx;
        let lr = arena.get(left_slot).right_idx;
        let llh = ll.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
        let lrh = lr.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
        let left_balance = MemNode::calc_balance_with(llh, lrh);

        // Update left_idx to point to the CoW'd slot
        arena.get_mut(slot).left_idx = Some(NodeIdx::mem(current_gen, left_slot));

        if left_balance >= 0 {
            return rotate_right_arena(arena, frozen, snapshot, current_gen, slot, version);
        }
        // Left-right case
        let lr_node = arena.get(left_slot).right_idx.unwrap();
        let lr_slot =
            cow_any_to_mutable(arena, frozen, snapshot, current_gen, lr_node, version, cow_version);
        arena.get_mut(left_slot).right_idx = Some(NodeIdx::mem(current_gen, lr_slot));

        let rotated_left =
            rotate_left_arena(arena, frozen, snapshot, current_gen, left_slot, version);
        arena.get_mut(slot).left_idx = Some(NodeIdx::mem(current_gen, rotated_left));
        return rotate_right_arena(arena, frozen, snapshot, current_gen, slot, version);
    }
    if balance < -1 {
        // Right-heavy
        let right = right_idx.expect("right must exist for balance < -1");
        let right_slot =
            cow_any_to_mutable(arena, frozen, snapshot, current_gen, right, version, cow_version);
        let rl = arena.get(right_slot).left_idx;
        let rr = arena.get(right_slot).right_idx;
        let rlh = rl.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
        let rrh = rr.map_or(0, |i| read_height_size(arena, frozen, snapshot, current_gen, i).0);
        let right_balance = MemNode::calc_balance_with(rlh, rrh);

        arena.get_mut(slot).right_idx = Some(NodeIdx::mem(current_gen, right_slot));

        if right_balance <= 0 {
            return rotate_left_arena(arena, frozen, snapshot, current_gen, slot, version);
        }
        // Right-left case
        let rl_node = arena.get(right_slot).left_idx.unwrap();
        let rl_slot =
            cow_any_to_mutable(arena, frozen, snapshot, current_gen, rl_node, version, cow_version);
        arena.get_mut(right_slot).left_idx = Some(NodeIdx::mem(current_gen, rl_slot));

        let rotated_right =
            rotate_right_arena(arena, frozen, snapshot, current_gen, right_slot, version);
        arena.get_mut(slot).right_idx = Some(NodeIdx::mem(current_gen, rotated_right));
        return rotate_left_arena(arena, frozen, snapshot, current_gen, slot, version);
    }
    slot
}

/// Right rotation in arena: promotes left child as new root.
/// `slot` must be in the mutable arena.
/// Returns the arena slot of the new root.
fn rotate_right_arena(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    slot: u32,
    _version: u32,
) -> u32 {
    let left_idx = arena.get(slot).left_idx.expect("rotate_right requires left child");
    // left_idx should already be in mutable arena (CoW'd by rebalance_arena)
    let left_slot = if left_idx.generation == current_gen {
        left_idx.index
    } else {
        panic!("rotate_right_arena: left child should already be in mutable arena")
    };

    // LR becomes S's new left child
    let lr = arena.get(left_slot).right_idx;
    arena.get_mut(slot).left_idx = lr;

    // Update S's height/size
    let s_left = arena.get(slot).left_idx;
    let s_right = arena.get(slot).right_idx;
    let (slh, sls) =
        s_left.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    let (srh, srs) =
        s_right.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    arena.get_mut(slot).update_height_size_with(slh, srh, sls, srs);

    // S becomes new_root's right child
    let s_height = arena.get(slot).height;
    let s_size = arena.get(slot).size;
    arena.get_mut(left_slot).right_idx = Some(NodeIdx::mem(current_gen, slot));
    // Key doesn't change: L.key was leftmost(LR), and after rotation
    // L's right subtree still starts with LR (now as S.left), so
    // leftmost of L's right subtree = leftmost(S) = leftmost(LR) = L.key

    // Update new_root's height/size
    let nr_left = arena.get(left_slot).left_idx;
    let (nrlh, nrls) =
        nr_left.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    arena.get_mut(left_slot).update_height_size_with(nrlh, s_height, nrls, s_size);

    left_slot
}

/// Left rotation in arena: promotes right child as new root.
/// `slot` must be in the mutable arena.
/// Returns the arena slot of the new root.
fn rotate_left_arena(
    arena: &mut MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    slot: u32,
    _version: u32,
) -> u32 {
    let right_idx = arena.get(slot).right_idx.expect("rotate_left requires right child");
    let right_slot = if right_idx.generation == current_gen {
        right_idx.index
    } else {
        panic!("rotate_left_arena: right child should already be in mutable arena")
    };

    // RL becomes S's new right child
    let rl = arena.get(right_slot).left_idx;
    arena.get_mut(slot).right_idx = rl;
    // Key doesn't change: S.key was the separator between SL and R.
    // RL (now S's right child) was part of R, so all keys in RL >= S.key.
    // SL keys < S.key. So S.key remains a valid separator.

    // Update S's height/size
    let s_left = arena.get(slot).left_idx;
    let s_right = arena.get(slot).right_idx;
    let (slh, sls) =
        s_left.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    let (srh, srs) =
        s_right.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    arena.get_mut(slot).update_height_size_with(slh, srh, sls, srs);

    // S becomes new_root's left child
    let s_height = arena.get(slot).height;
    let s_size = arena.get(slot).size;
    arena.get_mut(right_slot).left_idx = Some(NodeIdx::mem(current_gen, slot));

    // Update new_root's height/size
    let nr_right = arena.get(right_slot).right_idx;
    let (nrrh, nrrs) =
        nr_right.map_or((0, 0), |i| read_height_size(arena, frozen, snapshot, current_gen, i));
    arena.get_mut(right_slot).update_height_size_with(s_height, nrrh, s_size, nrrs);

    right_slot
}

/// Look up a key in the arena-based tree.
/// Returns `Some((value, index))` if found.
pub fn get_arena(
    arena: &MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    node: Option<NodeIdx>,
    key: &[u8],
) -> Option<(Vec<u8>, u32)> {
    let idx = node?;

    if idx.is_persisted() {
        let snap = snapshot.as_ref()?;
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        let (val, i) = pn.get(key);
        return val.map(|v| (v, i));
    }

    let n = resolve_mem_node(arena, frozen, current_gen, idx);

    if n.height == 0 {
        // Leaf
        return match n.key.as_slice().cmp(key) {
            Ordering::Equal => Some((n.value.clone(), 0)),
            _ => None,
        };
    }

    if key < n.key.as_slice() {
        return get_arena(arena, frozen, snapshot, current_gen, n.left_idx, key);
    }

    // Search right
    let right_idx = n.right_idx;
    if let Some(right) = right_idx {
        if let Some((value, i)) = get_arena(arena, frozen, snapshot, current_gen, Some(right), key)
        {
            let (_, rs) = read_height_size(arena, frozen, snapshot, current_gen, right);
            let left_size = n.size - rs;
            return Some((value, i + left_size as u32));
        }
    }
    None
}

/// Compute the hash of a node in the arena, recursively computing children's hashes.
pub fn compute_hash_recursive(
    arena: &MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
) -> [u8; 32] {
    if idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required");
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        let hash = pn.hash();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(hash);
        return arr;
    }

    let node = resolve_mem_node(arena, frozen, current_gen, idx);

    // Check cached hash
    if let Some(h) = node.hash.get() {
        return *h;
    }

    // Compute hash
    let hash = crate::memiavl::node::compute_hash_arena(node, |child_idx| {
        compute_hash_recursive(arena, frozen, snapshot, current_gen, child_idx)
    });

    // Try to cache (best-effort; if concurrent, one wins)
    let _ = node.hash.set(hash);
    hash
}

/// Build a NodeRef (Arc<Node>) from a NodeIdx for backward compatibility.
///
/// This is used by consumers (iterator, proof, snapshot_writer) that
/// haven't been migrated to arena-based APIs yet.
#[allow(clippy::arc_with_non_send_sync)]
pub fn idx_to_node_ref(
    arena: &MutableArena,
    frozen: &[Arc<FrozenArena>],
    snapshot: &Option<Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
) -> NodeRef {
    if idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required");
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        return Arc::new(Node::Persisted(pn));
    }

    let node = resolve_mem_node(arena, frozen, current_gen, idx);

    // Recursively build left and right NodeRefs
    let left = node.left_idx.map(|i| idx_to_node_ref(arena, frozen, snapshot, current_gen, i));
    let right = node.right_idx.map(|i| idx_to_node_ref(arena, frozen, snapshot, current_gen, i));

    // Also check legacy fields
    let left = left.or_else(|| node.left.clone());
    let right = right.or_else(|| node.right.clone());

    let mem = MemNode {
        height: node.height,
        version: node.version,
        size: node.size,
        key: node.key.clone(),
        value: node.value.clone(),
        left_idx: node.left_idx,
        right_idx: node.right_idx,
        left,
        right,
        hash: node.hash.clone(),
    };
    Arc::new(Node::Mem(mem))
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

    // -----------------------------------------------------------------------
    // Arena-based tests
    // -----------------------------------------------------------------------

    fn arena_build_tree(keys: &[&[u8]], version: u32) -> (MutableArena, Option<NodeIdx>) {
        let mut arena = MutableArena::new();
        let current_gen = 1u16;
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;

        let mut root: Option<NodeIdx> = None;
        for k in keys {
            let (new_root, _) = set_recursive_arena(
                &mut arena,
                &frozen,
                &snapshot,
                current_gen,
                root,
                k,
                k,
                version,
                0,
            );
            root = Some(new_root);
        }
        (arena, root)
    }

    #[test]
    fn test_arena_set_into_empty() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;

        let (root_idx, updated) =
            set_recursive_arena(&mut arena, &frozen, &snapshot, 1, None, b"hello", b"world", 1, 0);
        assert!(!updated);
        let node = arena.get(root_idx.index);
        assert_eq!(node.height, 0);
        assert_eq!(node.key, b"hello");
        assert_eq!(node.value, b"world");
    }

    #[test]
    fn test_arena_set_update() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;

        let (root, _) =
            set_recursive_arena(&mut arena, &frozen, &snapshot, 1, None, b"key", b"val1", 1, 0);
        let (root, updated) = set_recursive_arena(
            &mut arena,
            &frozen,
            &snapshot,
            1,
            Some(root),
            b"key",
            b"val2",
            2,
            0,
        );
        assert!(updated);
        let node = arena.get(root.index);
        assert_eq!(node.key, b"key");
        assert_eq!(node.value, b"val2");
    }

    #[test]
    fn test_arena_set_three_keys() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;

        let (root, _) =
            set_recursive_arena(&mut arena, &frozen, &snapshot, 1, None, b"bbb", b"v2", 1, 0);
        let (root, _) =
            set_recursive_arena(&mut arena, &frozen, &snapshot, 1, Some(root), b"aaa", b"v1", 1, 0);
        let (root, _) =
            set_recursive_arena(&mut arena, &frozen, &snapshot, 1, Some(root), b"ccc", b"v3", 1, 0);

        // Verify all keys via get_arena
        let r = get_arena(&arena, &frozen, &snapshot, 1, Some(root), b"aaa");
        assert_eq!(r.unwrap().0, b"v1");
        let r = get_arena(&arena, &frozen, &snapshot, 1, Some(root), b"bbb");
        assert_eq!(r.unwrap().0, b"v2");
        let r = get_arena(&arena, &frozen, &snapshot, 1, Some(root), b"ccc");
        assert_eq!(r.unwrap().0, b"v3");
        let r = get_arena(&arena, &frozen, &snapshot, 1, Some(root), b"zzz");
        assert!(r.is_none());
    }

    #[test]
    fn test_arena_balance_all_cases() {
        // LL case: c, b, a
        let (arena, root) = arena_build_tree(&[b"c", b"b", b"a"], 1);
        let r = root.unwrap();
        assert!(arena.get(r.index).height <= 2);

        // RR case: a, b, c
        let (arena, root) = arena_build_tree(&[b"a", b"b", b"c"], 1);
        let r = root.unwrap();
        assert!(arena.get(r.index).height <= 2);

        // LR case: c, a, b
        let (arena, root) = arena_build_tree(&[b"c", b"a", b"b"], 1);
        let r = root.unwrap();
        assert!(arena.get(r.index).height <= 2);

        // RL case: a, c, b
        let (arena, root) = arena_build_tree(&[b"a", b"c", b"b"], 1);
        let r = root.unwrap();
        assert!(arena.get(r.index).height <= 2);
    }

    #[test]
    fn test_arena_set_many_balanced() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;
        let current_gen = 1u16;

        let mut root: Option<NodeIdx> = None;
        for i in 0u32..100 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            let (new_root, _) = set_recursive_arena(
                &mut arena,
                &frozen,
                &snapshot,
                current_gen,
                root,
                key.as_bytes(),
                val.as_bytes(),
                1,
                0,
            );
            root = Some(new_root);
        }

        let r = root.unwrap();
        let node = arena.get(r.index);
        assert_eq!(node.size, 100);

        let max_height = (1.44 * (102_f64).log2()).ceil() as u8;
        assert!(
            node.height <= max_height,
            "height {} exceeds AVL bound {}",
            node.height,
            max_height,
        );

        // Verify all keys
        for i in 0u32..100 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            let result = get_arena(&arena, &frozen, &snapshot, current_gen, root, key.as_bytes());
            assert_eq!(
                result.as_ref().map(|(v, _)| v.as_slice()),
                Some(val.as_bytes()),
                "key {} not found or wrong value",
                key,
            );
        }
    }

    #[test]
    fn test_arena_remove() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;
        let current_gen = 1u16;

        let (root, _) = set_recursive_arena(
            &mut arena,
            &frozen,
            &snapshot,
            current_gen,
            None,
            b"aaa",
            b"v1",
            1,
            0,
        );
        let (root, _) = set_recursive_arena(
            &mut arena,
            &frozen,
            &snapshot,
            current_gen,
            Some(root),
            b"bbb",
            b"v2",
            1,
            0,
        );
        let (root, _) = set_recursive_arena(
            &mut arena,
            &frozen,
            &snapshot,
            current_gen,
            Some(root),
            b"ccc",
            b"v3",
            1,
            0,
        );

        // Remove "aaa"
        let (removed, new_root, _) =
            remove_recursive_arena(&mut arena, &frozen, &snapshot, current_gen, root, b"aaa", 2, 0);
        assert_eq!(removed.unwrap(), b"v1");
        let new_root = new_root.unwrap();

        // "aaa" gone, "bbb" and "ccc" still present
        assert!(
            get_arena(&arena, &frozen, &snapshot, current_gen, Some(new_root), b"aaa").is_none()
        );
        assert!(
            get_arena(&arena, &frozen, &snapshot, current_gen, Some(new_root), b"bbb").is_some()
        );
        assert!(
            get_arena(&arena, &frozen, &snapshot, current_gen, Some(new_root), b"ccc").is_some()
        );
    }

    #[test]
    fn test_arena_remove_not_found() {
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;
        let current_gen = 1u16;

        let (root, _) = set_recursive_arena(
            &mut arena,
            &frozen,
            &snapshot,
            current_gen,
            None,
            b"aaa",
            b"v1",
            1,
            0,
        );

        let (removed, new_root, _) =
            remove_recursive_arena(&mut arena, &frozen, &snapshot, current_gen, root, b"zzz", 2, 0);
        assert!(removed.is_none());
        assert!(new_root.is_some());
    }

    #[test]
    fn test_arena_hash_matches_arc() {
        // Build same tree with both arena and Arc-based APIs, verify hashes match.
        let mut arena = MutableArena::new();
        let frozen: Vec<Arc<FrozenArena>> = vec![];
        let snapshot: Option<Arc<Snapshot>> = None;
        let current_gen = 1u16;

        let mut arena_root: Option<NodeIdx> = None;
        let mut arc_root: Option<NodeRef> = None;

        for i in 0u32..20 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            let (new_arena, _) = set_recursive_arena(
                &mut arena,
                &frozen,
                &snapshot,
                current_gen,
                arena_root,
                key.as_bytes(),
                val.as_bytes(),
                1,
                0,
            );
            arena_root = Some(new_arena);
            let (new_arc, _) = set_recursive(arc_root, key.as_bytes(), val.as_bytes(), 1, 0);
            arc_root = Some(new_arc);
        }

        let arena_hash =
            compute_hash_recursive(&arena, &frozen, &snapshot, current_gen, arena_root.unwrap());
        let arc_root_node = arc_root.unwrap();
        let arc_hash = arc_root_node.hash();

        assert_eq!(arena_hash.as_slice(), arc_hash, "arena and arc hashes must match");
    }
}
