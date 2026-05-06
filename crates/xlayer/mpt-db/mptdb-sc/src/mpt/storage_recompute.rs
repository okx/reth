use alloy_primitives::B256;
use rayon::prelude::*;

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    hash,
    node::{ChildRef, MptNode},
};

#[derive(Clone)]
struct FinalizedHashNode {
    idx: u32,
    hash: B256,
}

/// Parallel hash-only variant for account trie root (Branch with 16 children).
pub(crate) fn recompute_hash_only_parallel(
    arena: &mut MutableTrieArena,
    root: Option<u32>,
) -> B256 {
    match root {
        None => alloy_trie::EMPTY_ROOT_HASH,
        Some(root_idx) => {
            let (hash, finalized) = compute_node_hash_only_parallel_root(arena, root_idx);
            for node in &finalized {
                // Only cache hash — skip set_rlp to keep overlay lean.
                arena.set_hash(node.idx, node.hash);
            }
            hash
        }
    }
}

// ---------------------------------------------------------------------------
// Hash-only variants: same RLP encode + keccak logic, but skip dirty_blobs
// collection. Used in wal_first mode where blobs are never persisted.
// ---------------------------------------------------------------------------

fn encode_child_for_parent_hash_only(
    arena: &MutableTrieArena,
    child: &ChildRef,
) -> (Vec<u8>, Vec<FinalizedHashNode>) {
    match child {
        ChildRef::Arena(idx) => {
            let idx = *idx;
            if !arena.is_dirty(idx) {
                if let Some(hash) = arena.get_hash(idx) {
                    return (hash.to_vec(), Vec::new());
                }
                if let Some(rlp) = arena.get_rlp(idx) {
                    let hash = hash::hash_rlp(rlp);
                    let embed = if rlp.len() < 32 { rlp.clone() } else { hash.to_vec() };
                    return (embed, vec![FinalizedHashNode { idx, hash }]);
                }
            }
            let (_hash, embed, finalized) = compute_node_hash_only_inner(arena, idx);
            (embed, finalized)
        }
        ChildRef::Inline(rlp) => (rlp.clone(), Vec::new()),
        ChildRef::Hash(hash) => (hash.to_vec(), Vec::new()),
    }
}

fn compute_node_hash_only(arena: &MutableTrieArena, idx: u32) -> (B256, Vec<FinalizedHashNode>) {
    let (hash, _embed, finalized) = compute_node_hash_only_inner(arena, idx);
    (hash, finalized)
}

fn compute_node_hash_only_inner(
    arena: &MutableTrieArena,
    idx: u32,
) -> (B256, Vec<u8>, Vec<FinalizedHashNode>) {
    let mut finalized = Vec::new();

    let rlp = match arena.get(idx) {
        MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
        MptNode::Extension(ext) => {
            let (embed, child_finalized) = encode_child_for_parent_hash_only(arena, &ext.child);
            finalized.extend(child_finalized);
            encode_extension(&ext.nibbles, &embed)
        }
        MptNode::Branch(branch) => {
            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            for (slot, child) in branch.children.iter().enumerate() {
                if let Some(child) = child {
                    let (embed, child_finalized) = encode_child_for_parent_hash_only(arena, child);
                    children_bytes[slot] = Some(embed);
                    finalized.extend(child_finalized);
                }
            }
            encode_branch(&children_bytes, branch.value.as_deref())
        }
    };

    let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
    finalized.insert(0, FinalizedHashNode { idx, hash: node_hash });
    let embed = if rlp.len() < 32 { rlp } else { node_hash.to_vec() };
    (node_hash, embed, finalized)
}

fn compute_node_hash_only_parallel_root(
    arena: &MutableTrieArena,
    idx: u32,
) -> (B256, Vec<FinalizedHashNode>) {
    match arena.get(idx) {
        MptNode::Branch(branch) => {
            let child_slots: Vec<(usize, &ChildRef)> = branch
                .children
                .iter()
                .enumerate()
                .filter_map(|(slot, child)| child.as_ref().map(|c| (slot, c)))
                .collect();

            let child_results: Vec<(usize, Vec<u8>, Vec<FinalizedHashNode>)> = child_slots
                .into_par_iter()
                .map(|(slot, child)| {
                    let (embed, finalized) = encode_child_for_parent_hash_only(arena, child);
                    (slot, embed, finalized)
                })
                .collect();

            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            let mut finalized = Vec::new();
            for (slot, embed, child_finalized) in child_results {
                children_bytes[slot] = Some(embed);
                finalized.extend(child_finalized);
            }

            let rlp = encode_branch(&children_bytes, branch.value.as_deref());
            let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
            finalized.insert(0, FinalizedHashNode { idx, hash: node_hash });
            (node_hash, finalized)
        }
        _ => compute_node_hash_only(arena, idx),
    }
}
