use alloy_primitives::B256;
use rayon::prelude::*;

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    hash,
    node::{ChildRef, MptNode},
};

#[derive(Clone)]
pub(crate) struct FinalizedNode {
    pub idx: u32,
    pub rlp: Vec<u8>,
    pub hash: B256,
}

struct ChildArtifacts {
    embed: Vec<u8>,
    finalized_nodes: Vec<FinalizedNode>,
    dirty_blobs: Vec<(B256, Vec<u8>)>,
}

pub(crate) struct StorageRecomputeResult {
    pub root: B256,
    pub dirty_blobs: Vec<(B256, Vec<u8>)>,
}

struct NodeArtifacts {
    hash: B256,
    finalized_nodes: Vec<FinalizedNode>,
    dirty_blobs: Vec<(B256, Vec<u8>)>,
}

pub(crate) fn recompute(arena: &mut MutableTrieArena, root: Option<u32>) -> StorageRecomputeResult {
    match root {
        None => StorageRecomputeResult { root: alloy_trie::EMPTY_ROOT_HASH, dirty_blobs: vec![] },
        Some(root_idx) => {
            let artifacts = compute_node_artifacts(arena, root_idx);
            for node in &artifacts.finalized_nodes {
                arena.set_rlp(node.idx, node.rlp.clone());
                arena.set_hash(node.idx, node.hash);
            }
            StorageRecomputeResult { root: artifacts.hash, dirty_blobs: artifacts.dirty_blobs }
        }
    }
}

/// Hash-only variant: computes root hash and caches ONLY the hash on arena
/// nodes — RLP is NOT cached.  This keeps the arena's rlp_cache empty so
/// that the subsequent `freeze()` is cheaper (nothing to clear) and the
/// frozen clone for the background worker carries no extra weight.
///
/// RLP caching across blocks is pointless in hash_only mode because
/// `freeze()` clears the rlp_cache anyway.  Within a single hash
/// computation, each trie node is visited exactly once (no shared
/// subtrees in MPT), so the memoization provides no benefit.
pub(crate) fn recompute_hash_only(arena: &mut MutableTrieArena, root: Option<u32>) -> B256 {
    match root {
        None => alloy_trie::EMPTY_ROOT_HASH,
        Some(root_idx) => {
            let (hash, _root_rlp, nodes) = compute_node_hash_only(arena, root_idx);
            for (idx, h) in nodes {
                arena.set_hash(idx, h);
            }
            hash
        }
    }
}

/// Parallel hash-only variant for account trie root (Branch with 16 children).
pub(crate) fn recompute_hash_only_parallel(
    arena: &mut MutableTrieArena,
    root: Option<u32>,
) -> B256 {
    match root {
        None => alloy_trie::EMPTY_ROOT_HASH,
        Some(root_idx) => {
            let (hash, _root_rlp, nodes) = compute_node_hash_only_parallel_root(arena, root_idx);
            for (idx, h) in nodes {
                arena.set_hash(idx, h);
            }
            hash
        }
    }
}

/// Parallel variant: hash the root's children in parallel using rayon.
/// Only effective when the root is a Branch node (16 independent subtrees).
pub(crate) fn recompute_parallel(
    arena: &mut MutableTrieArena,
    root: Option<u32>,
) -> StorageRecomputeResult {
    match root {
        None => StorageRecomputeResult { root: alloy_trie::EMPTY_ROOT_HASH, dirty_blobs: vec![] },
        Some(root_idx) => {
            let artifacts = compute_node_artifacts_parallel_root(arena, root_idx);
            for node in &artifacts.finalized_nodes {
                arena.set_rlp(node.idx, node.rlp.clone());
                arena.set_hash(node.idx, node.hash);
            }
            StorageRecomputeResult { root: artifacts.hash, dirty_blobs: artifacts.dirty_blobs }
        }
    }
}

/// Parallel hash computation for a root-level Branch node.
/// Each of the 16 children is hashed independently using rayon,
/// then the root hash is computed from the 16 child results.
fn compute_node_artifacts_parallel_root(arena: &MutableTrieArena, idx: u32) -> NodeArtifacts {
    match arena.get(idx) {
        MptNode::Branch(branch) => {
            // Collect child refs so we can process them in parallel.
            let child_slots: Vec<(usize, &ChildRef)> = branch
                .children
                .iter()
                .enumerate()
                .filter_map(|(slot, child)| child.as_ref().map(|c| (slot, c)))
                .collect();

            // Parallel: hash each child subtree independently.
            let child_results: Vec<(usize, ChildArtifacts)> = child_slots
                .into_par_iter()
                .map(|(slot, child)| {
                    let artifacts = encode_child_for_parent_collect(arena, child);
                    (slot, artifacts)
                })
                .collect();

            // Merge results serially.
            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            let mut finalized_nodes = Vec::new();
            let mut dirty_blobs = Vec::new();
            for (slot, artifacts) in child_results {
                children_bytes[slot] = Some(artifacts.embed);
                finalized_nodes.extend(artifacts.finalized_nodes);
                dirty_blobs.extend(artifacts.dirty_blobs);
            }

            let rlp = encode_branch(&children_bytes, branch.value.as_deref());
            let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
            finalized_nodes.insert(0, FinalizedNode { idx, rlp: rlp.clone(), hash: node_hash });
            if arena.is_dirty(idx) {
                dirty_blobs.insert(0, (node_hash, rlp));
            }
            NodeArtifacts { hash: node_hash, finalized_nodes, dirty_blobs }
        }
        // Non-branch root: fall back to serial.
        _ => compute_node_artifacts(arena, idx),
    }
}

fn encode_child_for_parent_collect(arena: &MutableTrieArena, child: &ChildRef) -> ChildArtifacts {
    match child {
        ChildRef::Arena(idx) => {
            let idx = *idx;
            if !arena.is_dirty(idx) {
                if let Some(hash) = arena.get_hash(idx) {
                    return ChildArtifacts {
                        embed: hash.to_vec(),
                        finalized_nodes: Vec::new(),
                        dirty_blobs: Vec::new(),
                    };
                }
                if let Some(rlp) = arena.get_rlp(idx) {
                    let hash = hash::hash_rlp(rlp);
                    return ChildArtifacts {
                        embed: if rlp.len() < 32 { rlp.clone() } else { hash.to_vec() },
                        finalized_nodes: vec![FinalizedNode { idx, rlp: rlp.clone(), hash }],
                        dirty_blobs: Vec::new(),
                    };
                }
            }

            let node = compute_node_artifacts(arena, idx);
            let node_rlp =
                node.finalized_nodes.first().expect("root cache update must exist").rlp.clone();
            ChildArtifacts {
                embed: if node_rlp.len() < 32 { node_rlp } else { node.hash.to_vec() },
                finalized_nodes: node.finalized_nodes,
                dirty_blobs: node.dirty_blobs,
            }
        }
        ChildRef::Inline(rlp) => ChildArtifacts {
            embed: rlp.clone(),
            finalized_nodes: Vec::new(),
            dirty_blobs: Vec::new(),
        },
        ChildRef::Hash(hash) => ChildArtifacts {
            embed: hash.to_vec(),
            finalized_nodes: Vec::new(),
            dirty_blobs: Vec::new(),
        },
    }
}

fn compute_node_artifacts(arena: &MutableTrieArena, idx: u32) -> NodeArtifacts {
    let mut finalized_nodes = Vec::new();
    let mut dirty_blobs = Vec::new();

    let rlp = match arena.get(idx) {
        MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
        MptNode::Extension(ext) => {
            let child = encode_child_for_parent_collect(arena, &ext.child);
            finalized_nodes.extend(child.finalized_nodes);
            dirty_blobs.extend(child.dirty_blobs);
            encode_extension(&ext.nibbles, &child.embed)
        }
        MptNode::Branch(branch) => {
            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            for (slot, child) in branch.children.iter().enumerate() {
                if let Some(child) = child {
                    let artifacts = encode_child_for_parent_collect(arena, child);
                    children_bytes[slot] = Some(artifacts.embed);
                    finalized_nodes.extend(artifacts.finalized_nodes);
                    dirty_blobs.extend(artifacts.dirty_blobs);
                }
            }
            encode_branch(&children_bytes, branch.value.as_deref())
        }
    };

    let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
    finalized_nodes.insert(0, FinalizedNode { idx, rlp: rlp.clone(), hash: node_hash });
    if arena.is_dirty(idx) {
        dirty_blobs.insert(0, (node_hash, rlp));
    }

    NodeArtifacts { hash: node_hash, finalized_nodes, dirty_blobs }
}

// ---------------------------------------------------------------------------
// Hash-only variants: same RLP encode + keccak logic, but skip dirty_blobs
// collection. Used in wal_first mode where blobs are never persisted.
// ---------------------------------------------------------------------------

/// Returns `(embed_for_parent, nodes_to_cache)`.
/// `nodes_to_cache` is a lightweight `Vec<(u32, B256)>` — no RLP stored.
/// The RLP is computed locally to produce the hash / embed, then dropped.
fn encode_child_for_parent_hash_only(
    arena: &MutableTrieArena,
    child: &ChildRef,
) -> (Vec<u8>, Vec<(u32, B256)>) {
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
                    return (embed, vec![(idx, hash)]);
                }
            }
            let (hash, node_rlp, nodes) = compute_node_hash_only(arena, idx);
            let embed = if node_rlp.len() < 32 { node_rlp } else { hash.to_vec() };
            (embed, nodes)
        }
        ChildRef::Inline(rlp) => (rlp.clone(), Vec::new()),
        ChildRef::Hash(hash) => (hash.to_vec(), Vec::new()),
    }
}

/// Returns `(hash_of_this_node, rlp_of_this_node, nodes_to_cache)`.
///
/// The rlp is returned so the caller can compute the inline-vs-hash embed for
/// the parent without needing to store it in the collected nodes.  All nodes
/// in `nodes_to_cache` are lightweight `(idx, hash)` pairs — no RLP stored.
fn compute_node_hash_only(arena: &MutableTrieArena, idx: u32) -> (B256, Vec<u8>, Vec<(u32, B256)>) {
    let mut nodes: Vec<(u32, B256)> = Vec::new();

    let rlp = match arena.get(idx) {
        MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
        MptNode::Extension(ext) => {
            let (embed, child_nodes) = encode_child_for_parent_hash_only(arena, &ext.child);
            nodes.extend(child_nodes);
            encode_extension(&ext.nibbles, &embed)
        }
        MptNode::Branch(branch) => {
            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            for (slot, child) in branch.children.iter().enumerate() {
                if let Some(child) = child {
                    let (embed, child_nodes) = encode_child_for_parent_hash_only(arena, child);
                    children_bytes[slot] = Some(embed);
                    nodes.extend(child_nodes);
                }
            }
            encode_branch(&children_bytes, branch.value.as_deref())
        }
    };

    let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
    nodes.push((idx, node_hash));
    (node_hash, rlp, nodes)
}

fn compute_node_hash_only_parallel_root(
    arena: &MutableTrieArena,
    idx: u32,
) -> (B256, Vec<u8>, Vec<(u32, B256)>) {
    match arena.get(idx) {
        MptNode::Branch(branch) => {
            let child_slots: Vec<(usize, &ChildRef)> = branch
                .children
                .iter()
                .enumerate()
                .filter_map(|(slot, child)| child.as_ref().map(|c| (slot, c)))
                .collect();

            let child_results: Vec<(usize, Vec<u8>, Vec<(u32, B256)>)> = child_slots
                .into_par_iter()
                .map(|(slot, child)| {
                    let (embed, nodes) = encode_child_for_parent_hash_only(arena, child);
                    (slot, embed, nodes)
                })
                .collect();

            let mut children_bytes: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            let mut nodes: Vec<(u32, B256)> = Vec::new();
            for (slot, embed, child_nodes) in child_results {
                children_bytes[slot] = Some(embed);
                nodes.extend(child_nodes);
            }

            let rlp = encode_branch(&children_bytes, branch.value.as_deref());
            let node_hash = arena.get_hash(idx).unwrap_or_else(|| hash::hash_rlp(&rlp));
            nodes.push((idx, node_hash));
            (node_hash, rlp, nodes)
        }
        _ => compute_node_hash_only(arena, idx),
    }
}
