use crate::memiavl::{
    arena::{resolve_mem_node, FrozenArena, MutableArena, NodeIdx},
    node::{Node, NodeRef},
    snapshot::Snapshot,
    tree::Tree,
    tree_algo::compute_hash_recursive,
};
use ics23::{
    commitment_proof::Proof, CommitmentProof, ExistenceProof, HashOp, InnerOp, LeafOp, LengthOp,
    NonExistenceProof,
};
use seidb_common::error::{Result, SeiDbError};

// ---------------------------------------------------------------------------
// Path-to-leaf helpers
// ---------------------------------------------------------------------------

/// An entry in the path from root to a target leaf.
/// Captures the sibling hash and which side the traversal went.
struct PathNode {
    /// Height of this inner node.
    height: i8,
    /// Size (total leaf count) of the subtree rooted at this inner node.
    size: i64,
    /// Version of this inner node.
    version: i64,
    /// Hash of the left child (non-empty when we went right).
    left: Vec<u8>,
    /// Hash of the right child (non-empty when we went left).
    right: Vec<u8>,
}

/// Walk from `node` down to the leaf matching `key`, collecting inner-node
/// path entries along the way. Returns `(path, leaf_node)` or an error if
/// the key is not found.
fn path_to_leaf(
    node: &NodeRef,
    key: &[u8],
) -> std::result::Result<(Vec<PathNode>, NodeRef), String> {
    let mut path = Vec::new();
    let mut current = node.clone();

    loop {
        let h = current.height();
        if h == 0 {
            // Leaf node -- check key match.
            if current.key() == key {
                return Ok((path, current));
            }
            return Err("key does not exist".to_string());
        }

        let height = h as i8;

        if key < current.key() {
            // Go left; record right sibling hash.
            let right = current.right().expect("branch node must have right child");
            path.push(PathNode {
                height,
                size: current.size(),
                version: current.version() as i64,
                left: Vec::new(),
                right: right.safe_hash(),
            });
            current = current.left().expect("branch node must have left child").clone();
        } else {
            // Go right; record left sibling hash.
            let left = current.left().expect("branch node must have left child");
            path.push(PathNode {
                height,
                size: current.size(),
                version: current.version() as i64,
                left: left.safe_hash(),
                right: Vec::new(),
            });
            current = current.right().expect("branch node must have right child").clone();
        }
    }
}

// ---------------------------------------------------------------------------
// Arena-based path-to-leaf
// ---------------------------------------------------------------------------

/// Arena-based version of [`path_to_leaf`].
fn path_to_leaf_arena(
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    start: NodeIdx,
    key: &[u8],
) -> std::result::Result<(Vec<PathNode>, NodeIdx), String> {
    let mut path = Vec::new();
    let mut current = start;

    loop {
        if current.is_persisted() {
            // Fall back to PersistedNode traversal
            let snap = snapshot.as_ref().expect("snapshot required");
            let pn = snap.node_at(current.persisted_index(), current.persisted_is_leaf());
            if pn.is_leaf() {
                if pn.key() == key {
                    return Ok((path, current));
                }
                return Err("key does not exist".to_string());
            }
            let h = pn.height() as i8;
            if key < pn.key() {
                let right_pn = pn.right();
                let right_hash_bytes = right_pn.hash();
                path.push(PathNode {
                    height: h,
                    size: pn.size(),
                    version: pn.version() as i64,
                    left: Vec::new(),
                    right: right_hash_bytes.to_vec(),
                });
                let left_pn = pn.left();
                current = NodeIdx::persisted(left_pn.index, left_pn.is_leaf);
            } else {
                let left_pn = pn.left();
                let left_hash_bytes = left_pn.hash();
                path.push(PathNode {
                    height: h,
                    size: pn.size(),
                    version: pn.version() as i64,
                    left: left_hash_bytes.to_vec(),
                    right: Vec::new(),
                });
                let right_pn = pn.right();
                current = NodeIdx::persisted(right_pn.index, right_pn.is_leaf);
            }
            continue;
        }

        let n = resolve_mem_node(arena, frozen, current_gen, current);

        if n.height == 0 {
            if n.key.as_slice() == key {
                return Ok((path, current));
            }
            return Err("key does not exist".to_string());
        }

        let h = n.height as i8;
        if key < n.key.as_slice() {
            // Go left; record right sibling hash
            let right_idx = n.right_idx.expect("branch must have right child");
            let right_hash =
                compute_hash_recursive(arena, frozen, snapshot, current_gen, right_idx);
            path.push(PathNode {
                height: h,
                size: n.size,
                version: n.version as i64,
                left: Vec::new(),
                right: right_hash.to_vec(),
            });
            current = n.left_idx.expect("branch must have left child");
        } else {
            // Go right; record left sibling hash
            let left_idx = n.left_idx.expect("branch must have left child");
            let left_hash = compute_hash_recursive(arena, frozen, snapshot, current_gen, left_idx);
            path.push(PathNode {
                height: h,
                size: n.size,
                version: n.version as i64,
                left: left_hash.to_vec(),
                right: Vec::new(),
            });
            current = n.right_idx.expect("branch must have right child");
        }
    }
}

/// Arena-based existence proof creation.
fn create_existence_proof_arena(
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    root: NodeIdx,
    key: &[u8],
) -> std::result::Result<ExistenceProof, String> {
    let (path, leaf_idx) = path_to_leaf_arena(arena, frozen, snapshot, current_gen, root, key)?;

    // Read leaf data
    let (leaf_key, leaf_value, leaf_version) = if leaf_idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required");
        let pn = snap.node_at(leaf_idx.persisted_index(), leaf_idx.persisted_is_leaf());
        (pn.key().to_vec(), pn.value().unwrap_or(&[]).to_vec(), pn.version() as i64)
    } else {
        let n = resolve_mem_node(arena, frozen, current_gen, leaf_idx);
        (n.key.clone(), n.value.clone(), n.version as i64)
    };

    Ok(ExistenceProof {
        key: leaf_key,
        value: leaf_value,
        leaf: Some(convert_leaf_op(leaf_version)),
        path: convert_inner_ops(&path),
    })
}

/// Arena-based get_with_index (returns insertion-point index for missing keys).
fn get_with_index_arena(
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
    key: &[u8],
) -> (Option<Vec<u8>>, u32) {
    if idx.is_persisted() {
        let snap = snapshot.as_ref().expect("snapshot required");
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        // Delegate to PersistedNode which has its own get logic
        let node = Node::Persisted(pn);
        return get_with_index_node(&node, key);
    }

    let n = resolve_mem_node(arena, frozen, current_gen, idx);

    if n.height == 0 {
        return match n.key.as_slice().cmp(key) {
            std::cmp::Ordering::Less => (None, 1),
            std::cmp::Ordering::Greater => (None, 0),
            std::cmp::Ordering::Equal => (Some(n.value.clone()), 0),
        };
    }

    if key < n.key.as_slice() {
        if let Some(left) = n.left_idx {
            return get_with_index_arena(arena, frozen, snapshot, current_gen, left, key);
        }
        return (None, 0);
    }

    if let Some(right) = n.right_idx {
        let (value, i) = get_with_index_arena(arena, frozen, snapshot, current_gen, right, key);
        let right_n = resolve_mem_node(arena, frozen, current_gen, right);
        let left_size = n.size - right_n.size;
        return (value, i + left_size as u32);
    }
    (None, 0)
}

/// Arena-based get_by_index.
pub(crate) fn get_by_index_arena(
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    idx: NodeIdx,
    index: i64,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if idx.is_persisted() {
        let snap = snapshot.as_ref()?;
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        if index < 0 {
            return None;
        }
        return pn.get_by_index(index as u32);
    }

    let n = resolve_mem_node(arena, frozen, current_gen, idx);

    if n.height == 0 {
        return if index == 0 { Some((n.key.clone(), n.value.clone())) } else { None };
    }

    if let Some(left) = n.left_idx {
        let left_n = resolve_mem_node(arena, frozen, current_gen, left);
        let left_size = left_n.size;
        if index < left_size {
            return get_by_index_arena(arena, frozen, snapshot, current_gen, left, index);
        }
        if let Some(right) = n.right_idx {
            return get_by_index_arena(
                arena,
                frozen,
                snapshot,
                current_gen,
                right,
                index - left_size,
            );
        }
    }
    None
}

// ---------------------------------------------------------------------------
// ICS23 proof construction helpers
// ---------------------------------------------------------------------------

/// Encode a signed 64-bit integer using Go's zigzag varint encoding
/// (`binary.PutVarint`).
fn convert_varint_to_bytes(value: i64) -> Vec<u8> {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    let mut buf = Vec::with_capacity(10);
    let mut v = zigzag;
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
    buf
}

/// Build the ICS23 `LeafOp` for an IAVL leaf at the given `version`.
///
/// The prefix encodes: `varint(height=0) || varint(size=1) || varint(version)`.
/// This matches the Go `convertLeafOp` function.
fn convert_leaf_op(version: i64) -> LeafOp {
    let mut prefix = convert_varint_to_bytes(0); // height = 0
    prefix.extend_from_slice(&convert_varint_to_bytes(1)); // size = 1
    prefix.extend_from_slice(&convert_varint_to_bytes(version));

    LeafOp {
        hash: HashOp::Sha256.into(),
        prehash_key: HashOp::NoHash.into(),
        prehash_value: HashOp::Sha256.into(),
        length: LengthOp::VarProto.into(),
        prefix,
    }
}

/// Convert the root-to-leaf path into a leaf-to-root sequence of `InnerOp`s.
///
/// This matches the Go `convertInnerOps` function. The path is reversed
/// because IAVL stores root-to-leaf but ICS23 expects leaf-to-root.
fn convert_inner_ops(path: &[PathNode]) -> Vec<InnerOp> {
    let length_byte: u8 = 0x20;
    let mut steps = Vec::with_capacity(path.len());

    // Reverse: IAVL path is root-to-leaf, ICS23 expects leaf-to-root.
    for pn in path.iter().rev() {
        let mut prefix = convert_varint_to_bytes(pn.height as i64);
        prefix.extend_from_slice(&convert_varint_to_bytes(pn.size));
        prefix.extend_from_slice(&convert_varint_to_bytes(pn.version));

        let suffix = if !pn.left.is_empty() {
            // We went right -- left sibling is known.
            prefix.push(length_byte);
            prefix.extend_from_slice(&pn.left);
            // Prepend the length prefix for the child (our hash).
            prefix.push(length_byte);
            Vec::new()
        } else {
            // We went left -- right sibling is known.
            // Prepend the length prefix for the child (our hash).
            prefix.push(length_byte);
            // Length-prefixed right sibling in suffix.
            let mut s = Vec::with_capacity(1 + pn.right.len());
            s.push(length_byte);
            s.extend_from_slice(&pn.right);
            s
        };

        steps.push(InnerOp { hash: HashOp::Sha256.into(), prefix, suffix });
    }
    steps
}

/// Create an `ExistenceProof` for the given key in the tree rooted at `root`.
fn create_existence_proof(
    root: &NodeRef,
    key: &[u8],
) -> std::result::Result<ExistenceProof, String> {
    let (path, leaf) = path_to_leaf(root, key)?;
    Ok(ExistenceProof {
        key: leaf.key().to_vec(),
        value: leaf.value().to_vec(),
        leaf: Some(convert_leaf_op(leaf.version() as i64)),
        path: convert_inner_ops(&path),
    })
}

// ---------------------------------------------------------------------------
// get_with_index -- returns the insertion-point index even for missing keys
// ---------------------------------------------------------------------------

/// Walk the tree and return `(value_if_found, index)`.
///
/// When the key is found, `index` is its in-order position.
/// When the key is NOT found, `index` is the position where the key would be
/// inserted (i.e. the index of the first key greater than `key`).
///
/// This matches Go's `MemNode.Get` semantics where a missing key still
/// returns a valid insertion-point index.
fn get_with_index_node(node: &Node, key: &[u8]) -> (Option<Vec<u8>>, u32) {
    if node.is_leaf() {
        return match node.key().cmp(key) {
            std::cmp::Ordering::Less => (None, 1),
            std::cmp::Ordering::Greater => (None, 0),
            std::cmp::Ordering::Equal => (Some(node.value().to_vec()), 0),
        };
    }

    if key < node.key() {
        if let Some(left) = node.left() {
            return get_with_index_node(left, key);
        }
        return (None, 0);
    }

    if let Some(right) = node.right() {
        let (value, idx) = get_with_index_node(right, key);
        let left_size = node.size() - right.size();
        return (value, idx + left_size as u32);
    }
    (None, 0)
}

// ---------------------------------------------------------------------------
// Public API on Tree
// ---------------------------------------------------------------------------

impl Tree {
    /// Generate an ICS23 membership proof that `key` exists in the tree.
    ///
    /// Returns an error if the key is not found or the tree is empty.
    pub fn get_membership_proof(&self, key: &[u8]) -> Result<CommitmentProof> {
        let root_idx =
            self.root_idx.ok_or_else(|| SeiDbError::Other("tree is empty".to_string()))?;
        let exist = create_existence_proof_arena(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            root_idx,
            key,
        )
        .map_err(SeiDbError::Other)?;
        Ok(CommitmentProof { proof: Some(Proof::Exist(exist)) })
    }

    /// Generate an ICS23 non-membership proof that `key` does NOT exist.
    ///
    /// Returns an error if the key actually exists or the tree is empty.
    pub fn get_non_membership_proof(&self, key: &[u8]) -> Result<CommitmentProof> {
        let root_idx =
            self.root_idx.ok_or_else(|| SeiDbError::Other("tree is empty".to_string()))?;

        let (val, idx) = get_with_index_arena(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            root_idx,
            key,
        );
        if val.is_some() {
            return Err(SeiDbError::Other(
                "cannot create NonExistenceProof when key is in state".to_string(),
            ));
        }

        let mut nonexist = NonExistenceProof { key: key.to_vec(), left: None, right: None };

        // Left neighbor: the key at index-1 (if it exists).
        if idx >= 1 &&
            let Some((left_key, _)) = get_by_index_arena(
                &self.arena,
                &self.frozen_arenas,
                &self.snapshot,
                self.current_gen,
                root_idx,
                (idx as i64) - 1,
            )
        {
            let left_proof = create_existence_proof_arena(
                &self.arena,
                &self.frozen_arenas,
                &self.snapshot,
                self.current_gen,
                root_idx,
                &left_key,
            )
            .map_err(SeiDbError::Other)?;
            nonexist.left = Some(left_proof);
        }

        // Right neighbor: the key at index `idx` (if it exists).
        if let Some((right_key, _)) = get_by_index_arena(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            root_idx,
            idx as i64,
        ) {
            let right_proof = create_existence_proof_arena(
                &self.arena,
                &self.frozen_arenas,
                &self.snapshot,
                self.current_gen,
                root_idx,
                &right_key,
            )
            .map_err(SeiDbError::Other)?;
            nonexist.right = Some(right_proof);
        }

        Ok(CommitmentProof { proof: Some(Proof::Nonexist(nonexist)) })
    }

    /// Verify an ICS23 membership proof against this tree's root hash.
    ///
    /// Returns `true` if the proof is valid for the given key.
    pub fn verify_membership(&self, proof: &CommitmentProof, key: &[u8]) -> bool {
        let val = match self.get(key) {
            Some(v) => v,
            None => return false,
        };
        let root = self.root_hash();
        let spec = iavl_spec();
        ics23::verify_membership::<Sha256HostFunctions>(proof, &spec, &root, key, &val)
    }

    /// Verify an ICS23 non-membership proof against this tree's root hash.
    ///
    /// Returns `true` if the proof is valid for the given key.
    pub fn verify_non_membership(&self, proof: &CommitmentProof, key: &[u8]) -> bool {
        let root = self.root_hash();
        let spec = iavl_spec();
        ics23::verify_non_membership::<Sha256HostFunctions>(proof, &spec, &root, key)
    }
}

// ---------------------------------------------------------------------------
// IAVL ProofSpec (matching Go's ics23.IavlSpec)
// ---------------------------------------------------------------------------

/// Returns the ICS23 `ProofSpec` for IAVL trees, matching Go's `ics23.IavlSpec`.
pub fn iavl_spec() -> ics23::ProofSpec {
    let leaf = LeafOp {
        hash: HashOp::Sha256.into(),
        prehash_key: HashOp::NoHash.into(),
        prehash_value: HashOp::Sha256.into(),
        length: LengthOp::VarProto.into(),
        prefix: vec![0u8],
    };
    let inner = ics23::InnerSpec {
        child_order: vec![0, 1],
        min_prefix_length: 4,
        max_prefix_length: 12,
        child_size: 33, // 1-byte length prefix + 32-byte SHA256 hash
        empty_child: vec![],
        hash: HashOp::Sha256.into(),
    };
    ics23::ProofSpec {
        leaf_spec: Some(leaf),
        inner_spec: Some(inner),
        min_depth: 0,
        max_depth: 0,
        prehash_key_before_comparison: false,
    }
}

// ---------------------------------------------------------------------------
// Minimal HostFunctionsProvider for SHA256-only verification
// ---------------------------------------------------------------------------

/// A minimal `HostFunctionsProvider` implementation that supports SHA256
/// (the only hash used by IAVL trees). Other hash functions panic because
/// they are never called for IAVL proof verification.
struct Sha256HostFunctions;

impl ics23::HostFunctionsProvider for Sha256HostFunctions {
    fn sha2_256(message: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(message);
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&digest);
        buf
    }

    fn sha2_512(_message: &[u8]) -> [u8; 64] {
        unimplemented!("SHA-512 not used by IAVL proofs")
    }

    fn sha2_512_truncated(_message: &[u8]) -> [u8; 32] {
        unimplemented!("SHA-512/256 not used by IAVL proofs")
    }

    fn keccak_256(_message: &[u8]) -> [u8; 32] {
        unimplemented!("Keccak-256 not used by IAVL proofs")
    }

    fn ripemd160(_message: &[u8]) -> [u8; 20] {
        unimplemented!("RIPEMD-160 not used by IAVL proofs")
    }

    fn blake2b_512(_message: &[u8]) -> [u8; 64] {
        unimplemented!("BLAKE2b-512 not used by IAVL proofs")
    }

    fn blake2s_256(_message: &[u8]) -> [u8; 32] {
        unimplemented!("BLAKE2s-256 not used by IAVL proofs")
    }

    fn blake3(_message: &[u8]) -> [u8; 32] {
        unimplemented!("BLAKE3 not used by IAVL proofs")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small tree for testing.
    fn build_test_tree() -> Tree {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"alice", b"100");
        tree.set(b"bob", b"200");
        tree.set(b"carol", b"300");
        tree.set(b"dave", b"400");
        tree.set(b"eve", b"500");
        tree.save_version(true).unwrap();
        tree
    }

    #[test]
    fn test_membership_proof_basic() {
        let tree = build_test_tree();

        // Generate and verify membership proof for each key.
        for (key, value) in [
            (&b"alice"[..], &b"100"[..]),
            (b"bob", b"200"),
            (b"carol", b"300"),
            (b"dave", b"400"),
            (b"eve", b"500"),
        ] {
            let proof = tree
                .get_membership_proof(key)
                .unwrap_or_else(|_| panic!("should generate proof for {:?}", key));

            // Proof should be an ExistenceProof.
            assert!(
                matches!(&proof.proof, Some(Proof::Exist(_))),
                "expected ExistenceProof for key {:?}",
                key,
            );

            // Verify against the tree's root hash.
            assert!(
                tree.verify_membership(&proof, key),
                "membership verification failed for {:?}",
                key,
            );

            // Verify manually: the proof's key/value should match.
            if let Some(Proof::Exist(ep)) = &proof.proof {
                assert_eq!(ep.key, key);
                assert_eq!(ep.value, value);
            }
        }
    }

    #[test]
    fn test_membership_proof_missing_key() {
        let tree = build_test_tree();

        // Key not in tree should fail.
        let result = tree.get_membership_proof(b"frank");
        assert!(result.is_err());
    }

    #[test]
    fn test_non_membership_proof_basic() {
        let tree = build_test_tree();

        // Keys that don't exist in the tree.
        let missing_keys: &[&[u8]] = &[
            b"aaa",   // before all keys
            b"betty", // between bob and carol
            b"diana", // between dave and eve
            b"zzz",   // after all keys
        ];

        for key in missing_keys {
            let proof = tree
                .get_non_membership_proof(key)
                .unwrap_or_else(|_| panic!("should generate non-membership proof for {:?}", key));

            // Proof should be a NonExistenceProof.
            assert!(
                matches!(&proof.proof, Some(Proof::Nonexist(_))),
                "expected NonExistenceProof for key {:?}",
                key,
            );

            // Verify against the tree's root hash.
            assert!(
                tree.verify_non_membership(&proof, key),
                "non-membership verification failed for {:?}",
                key,
            );
        }
    }

    #[test]
    fn test_non_membership_proof_existing_key() {
        let tree = build_test_tree();

        // Attempting non-membership proof for an existing key should fail.
        let result = tree.get_non_membership_proof(b"alice");
        assert!(result.is_err());
    }

    #[test]
    fn test_proof_empty_tree() {
        let tree = Tree::new_empty(0, 0);

        let result = tree.get_membership_proof(b"any");
        assert!(result.is_err(), "membership proof on empty tree should fail");

        let result = tree.get_non_membership_proof(b"any");
        assert!(result.is_err(), "non-membership proof on empty tree should fail");
    }

    #[test]
    fn test_membership_proof_single_key() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"only", b"one");
        tree.save_version(true).unwrap();

        let proof = tree.get_membership_proof(b"only").unwrap();
        assert!(tree.verify_membership(&proof, b"only"));
    }

    #[test]
    fn test_non_membership_proof_single_key() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"only", b"one");
        tree.save_version(true).unwrap();

        // Before the only key.
        let proof = tree.get_non_membership_proof(b"aaa").unwrap();
        assert!(tree.verify_non_membership(&proof, b"aaa"));

        // After the only key.
        let proof = tree.get_non_membership_proof(b"zzz").unwrap();
        assert!(tree.verify_non_membership(&proof, b"zzz"));
    }

    #[test]
    fn test_proof_two_keys() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"left", b"l");
        tree.set(b"right", b"r");
        tree.save_version(true).unwrap();

        // Membership proofs.
        let p1 = tree.get_membership_proof(b"left").unwrap();
        assert!(tree.verify_membership(&p1, b"left"));

        let p2 = tree.get_membership_proof(b"right").unwrap();
        assert!(tree.verify_membership(&p2, b"right"));

        // Non-membership: between the two keys.
        let p3 = tree.get_non_membership_proof(b"middle").unwrap();
        assert!(tree.verify_non_membership(&p3, b"middle"));
    }

    #[test]
    fn test_iavl_spec() {
        let spec = iavl_spec();
        assert!(spec.leaf_spec.is_some());
        assert!(spec.inner_spec.is_some());

        let leaf = spec.leaf_spec.unwrap();
        assert_eq!(leaf.hash, i32::from(HashOp::Sha256));
        assert_eq!(leaf.prehash_value, i32::from(HashOp::Sha256));
        assert_eq!(leaf.length, i32::from(LengthOp::VarProto));

        let inner = spec.inner_spec.unwrap();
        assert_eq!(inner.child_order, vec![0, 1]);
        assert_eq!(inner.child_size, 33);
    }

    #[test]
    fn test_convert_varint_to_bytes() {
        // 0 encodes as zigzag 0 => [0x00].
        assert_eq!(convert_varint_to_bytes(0), vec![0x00]);
        // 1 encodes as zigzag 2 => [0x02].
        assert_eq!(convert_varint_to_bytes(1), vec![0x02]);
        // -1 encodes as zigzag 1 => [0x01].
        assert_eq!(convert_varint_to_bytes(-1), vec![0x01]);
    }
}
