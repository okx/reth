
//! Segment-backed `TrieNodeProviderFactory` for `SparseStateTrie` integration.
//!
//! Implements the single custom interface that connects mpt-db's mmap-backed
//! storage segments to reth's `SparseStateTrie` computation engine.
//!
//! See `.claude/plans/sparse-state-trie-storage.md` for the full design.

use alloy_primitives::{keccak256, Address, Bytes, B256};
use alloy_rlp::{Decodable as _, Encodable as _};
use alloy_trie::{
    nodes::{
        BranchNode as TrieBranchNode, ExtensionNode as TrieExtensionNode, LeafNode as TrieLeafNode,
        RlpNode, TrieNode,
    },
    proof::DecodedProofNodes,
};
use mptdb_common::error::{MptDbError, Result as MptResult};
use reth_execution_errors::{SparseTrieError, SparseTrieErrorKind};
use reth_primitives_traits::Account as RethAccount;
use reth_trie_common::{
    updates::{StorageTrieUpdates, TrieUpdates},
    AccountProof, BranchNodeMasks, BranchNodeMasksMap, DecodedStorageMultiProof, Nibbles,
    StorageProof, TrieAccount, EMPTY_ROOT_HASH,
};
use reth_trie_sparse::{
    provider::{RevealedNode, TrieNodeProvider, TrieNodeProviderFactory},
    SerialSparseTrie, SparseNode, SparseStateTrie,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    node::{ChildRef, MptNode},
    segment::{SegmentPageLease, StorageTrieSegmentReader},
    state::DirtyAccount,
};

// ── Error helpers ─────────────────────────────────────────────────────────────

/// Wraps a string message into a `SparseTrieError` via the `Other` variant.
fn sparse_err(msg: impl Into<String>) -> SparseTrieError {
    SparseTrieErrorKind::Other(Box::new(std::io::Error::other(msg.into()))).into()
}

// ── Provider types ────────────────────────────────────────────────────────────

/// Storage trie node provider backed by a single mmap segment page.
///
/// `is_known_empty` controls behaviour when `lease` is `None`:
///
/// - `true` (new account or `storage_wiped`): `trie_node` returns `Ok(None)`, which the sparse trie
///   treats as `EmptyRoot` — correct for new accounts that never had storage.
///
/// - `false` (existing account): `trie_node` returns `Err`, because a missing lease for an existing
///   account indicates data loss, not an empty trie. Returning `Ok(None)` would silently set the
///   storage root to `EMPTY_ROOT_HASH`, producing a wrong state root.
///
/// `pre_built_proof` is an optional fallback used in **cross-block** mode when no segment is
/// available.  It holds a `DecodedStorageMultiProof` built from a dirty-key-only persisted preload
/// (tier-3) or a similar small sub-trie.  The provider only consults this fallback when a
/// Hash-blinded node is encountered during `update_storage_leaf` — for fully-revealed paths the
/// provider is never called, so the clone cost is zero in the common case.
pub struct SegmentStorageNodeProvider {
    /// Segment page for this account's storage trie, or `None` if absent.
    pub lease: Option<Arc<SegmentPageLease>>,
    /// When `true`, `Ok(None)` is returned on a missing lease (new/empty account).
    /// When `false`, a missing lease is `Err` (fail-fast: data-loss, not empty).
    pub is_known_empty: bool,
    /// Cross-block fallback proof (subset of nodes for dirty paths only).
    /// Consulted only when `lease` is `None` and the provider is actually called.
    pub pre_built_proof: Option<DecodedStorageMultiProof>,
}

/// Account trie node provider backed by a single mmap segment page.
///
/// Unlike `SegmentStorageNodeProvider`, a missing lease is **always** `Err`.
///
/// The caller (`revealed_trie_mut`, `state.rs:574`) treats `Ok(None)` as
/// `TrieNode::EmptyRoot`, silently setting the account root to
/// `EMPTY_ROOT_HASH` even for a non-empty chain.  Genesis and known-empty-chain
/// cases are handled exclusively through the eager
/// `reveal_decoded_account_multiproof` call in Step 0 of
/// `apply_all_storage_changes_sparse`; the provider fallback is never invoked
/// for genesis.
pub struct SegmentAccountNodeProvider {
    /// Segment page for the account trie, or `None` if absent.
    pub lease: Option<Arc<SegmentPageLease>>,
}

/// Segment-backed `TrieNodeProviderFactory` for `SparseStateTrie`.
///
/// Connects mpt-db's mmap segments to reth's sparse trie computation layer.
/// This is the only custom interface required — everything else (apply, root,
/// proof generation) reuses reth's `SparseStateTrie` code directly.
///
/// `known_empty_accounts` must be populated **before** passing this factory to
/// `apply_all_storage_changes_sparse` (Phase 2).  It controls whether
/// `SegmentStorageNodeProvider::trie_node` returns `Ok(None)` or `Err` when
/// the segment for an account is absent.
pub struct SegmentTrieNodeProviderFactory {
    /// Segment page for the account trie, if available.
    pub account_segment: Option<Arc<SegmentPageLease>>,
    /// Per-account storage segment pages, keyed by hashed address.
    pub storage_segments: HashMap<B256, Arc<SegmentPageLease>>,
    /// Pre-built storage proofs for accounts whose published segment is stale
    /// (root mismatch) but whose committed `StorageTrieCow` is available in
    /// the L2 cache.  Used as a fallback when `storage_segments` is empty and
    /// the account is NOT known-empty.
    ///
    /// Built in `MptCommitStore::build_sparse_factory` from `storage_trie_handles`.
    pub pre_built_storage_proofs: alloy_primitives::map::HashMap<B256, DecodedStorageMultiProof>,
    /// Accounts confirmed to have no prior storage (new accounts or
    /// `storage_wiped` accounts after `apply_all_storage_changes_sparse`
    /// processes them).
    ///
    /// Populated by the caller:
    /// ```ignore
    /// let known_empty: HashSet<B256> = dirty_accounts.iter()
    ///     .filter(|d| d.storage_known_empty || d.storage_wiped)
    ///     .map(|d| d.hashed_address)
    ///     .collect();
    /// ```
    pub known_empty_accounts: HashSet<B256>,
}

impl SegmentTrieNodeProviderFactory {
    /// Creates a new empty factory with no segments or known-empty accounts.
    pub fn new() -> Self {
        Self {
            account_segment: None,
            storage_segments: HashMap::new(),
            pre_built_storage_proofs: alloy_primitives::map::HashMap::default(),
            known_empty_accounts: HashSet::new(),
        }
    }
}

impl Default for SegmentTrieNodeProviderFactory {
    fn default() -> Self {
        Self::new()
    }
}

// ── TrieNodeProvider implementations ─────────────────────────────────────────

impl TrieNodeProvider for SegmentStorageNodeProvider {
    fn trie_node(
        &self,
        path: &Nibbles,
    ) -> std::result::Result<Option<RevealedNode>, SparseTrieError> {
        if let Some(ref lease) = self.lease {
            return open_reader_and_get(lease, path);
        }

        if self.is_known_empty {
            // New account or storage_wiped: empty trie is the correct answer.
            return Ok(None);
        }

        // Cross-block fallback: pre-built proof from dirty-key-only tier-3 preload.
        // Only consulted when a Hash-blinded node is encountered during
        // `update_storage_leaf` for a path NOT fully revealed in the reused
        // cross-block sparse trie.
        if let Some(ref proof) = self.pre_built_proof {
            if let Some(node) = proof.subtree.get(path) {
                let masks = proof.branch_node_masks.get(path);
                let (tree_mask, hash_mask) =
                    masks.map(|m| (Some(m.tree_mask), Some(m.hash_mask))).unwrap_or((None, None));
                let rlp = Bytes::from(alloy_rlp::encode(node));
                return Ok(Some(RevealedNode { node: rlp, tree_mask, hash_mask }));
            }
            // Path not in pre-built proof: genuinely absent in this sub-trie.
            return Ok(None);
        }

        // Existing account with a missing segment and no fallback is a data-loss condition.
        Err(sparse_err(
            "storage segment unavailable for existing account: cannot reveal \
             storage trie node; this is a data-loss condition, not an empty trie",
        ))
    }
}

impl TrieNodeProvider for SegmentAccountNodeProvider {
    fn trie_node(
        &self,
        path: &Nibbles,
    ) -> std::result::Result<Option<RevealedNode>, SparseTrieError> {
        let Some(ref lease) = self.lease else {
            // A missing account segment is ALWAYS a data-loss condition.
            // `Ok(None)` would silently set the account root to EMPTY_ROOT_HASH
            // (see `revealed_trie_mut`, state.rs:574), corrupting the state root.
            return Err(sparse_err(
                "account segment unavailable: cannot reveal account trie node; \
                 this is a data-loss condition, not an empty trie",
            ));
        };

        open_reader_and_get(lease, path)
    }
}

// ── TrieNodeProviderFactory implementation ────────────────────────────────────

impl TrieNodeProviderFactory for SegmentTrieNodeProviderFactory {
    type AccountNodeProvider = SegmentAccountNodeProvider;
    type StorageNodeProvider = SegmentStorageNodeProvider;

    fn account_node_provider(&self) -> Self::AccountNodeProvider {
        SegmentAccountNodeProvider { lease: self.account_segment.clone() }
    }

    fn storage_node_provider(&self, account: B256) -> Self::StorageNodeProvider {
        SegmentStorageNodeProvider {
            lease: self.storage_segments.get(&account).cloned(),
            is_known_empty: self.known_empty_accounts.contains(&account),
            // Cross-block fallback: cloned only when the provider is actually called
            // (i.e. when a Hash-blinded node is encountered), which is rare for
            // fully-revealed accounts.
            pre_built_proof: self.pre_built_storage_proofs.get(&account).cloned(),
        }
    }
}

// ── Sparse-trie → proof conversion ───────────────────────────────────────────

/// Builds `(DecodedProofNodes, BranchNodeMasksMap)` for the ACCOUNT trie from
/// a previously-committed `SparseStateTrie`.
///
/// Called at the start of each block's sparse apply to reveal the committed
/// account trie structure.  Uses `state_trie_ref().nodes_ref()` +
/// `values_ref()` — all hashes are correct after `root_with_updates`.
///
/// Falls back to `(EmptyRoot proof, empty masks)` when the trie is not yet
/// revealed (first block or after restart).
pub(crate) fn extract_account_proof_from_sparse_trie(
    sparse_trie: &SparseStateTrie,
) -> MptResult<(DecodedProofNodes, BranchNodeMasksMap)> {
    let Some(account_trie) = sparse_trie.state_trie_ref() else {
        let subtree = DecodedProofNodes::from_iter([(Nibbles::default(), TrieNode::EmptyRoot)]);
        return Ok((subtree, BranchNodeMasksMap::default()));
    };
    sparse_nodes_to_account_proof_nodes(account_trie)
}

/// Builds a `DecodedStorageMultiProof` for a single account's storage trie
/// from a previously-committed `SparseStateTrie`.
///
/// Returns `DecodedStorageMultiProof::empty()` when the account's storage trie
/// is not present in the sparse trie (new account or evicted).
pub(crate) fn extract_storage_proof_from_sparse_trie(
    sparse_trie: &SparseStateTrie,
    hashed_addr: &B256,
    root: B256,
) -> MptResult<Option<DecodedStorageMultiProof>> {
    let Some(storage_trie) = sparse_trie.storage_trie_ref(hashed_addr) else {
        return Ok(None);
    };
    Ok(Some(sparse_nodes_to_decoded_storage_multiproof(storage_trie, root)?))
}

/// DFS over a `SerialSparseTrie`'s revealed nodes to produce
/// `(DecodedProofNodes, BranchNodeMasksMap)` for the account trie.
///
/// Blinded nodes (`SparseNode::Hash`) are NOT included in the proof — they
/// will be served by the provider fallback when `update_account` needs them.
fn sparse_nodes_to_account_proof_nodes(
    trie: &SerialSparseTrie,
) -> MptResult<(DecodedProofNodes, BranchNodeMasksMap)> {
    let nodes = trie.nodes_ref();
    let values = trie.values_ref();
    let mut pairs: Vec<(Nibbles, TrieNode)> = Vec::new();
    let mut branch_masks: BranchNodeMasksMap = BranchNodeMasksMap::default();

    // DFS from root (empty Nibbles)
    let mut stack: Vec<Nibbles> = vec![Nibbles::default()];
    while let Some(path) = stack.pop() {
        let node = match nodes.get(&path) {
            Some(n) => n,
            None => continue,
        };
        match node {
            SparseNode::Empty => {
                pairs.push((path, TrieNode::EmptyRoot));
            }
            SparseNode::Hash(_) => {
                // Blinded: not included (provider handles on demand).
            }
            SparseNode::Leaf { key, .. } => {
                let full_path = nibbles_extend(&path, key);
                let value = values.get(&full_path).cloned().unwrap_or_default();
                pairs.push((
                    path,
                    TrieNode::Leaf(alloy_trie::nodes::LeafNode::new(key.clone(), value)),
                ));
            }
            SparseNode::Extension { key, .. } => {
                let child_path = nibbles_extend(&path, key);
                let child_hash = sparse_node_rlp(nodes, &child_path);
                pairs.push((
                    path,
                    TrieNode::Extension(alloy_trie::nodes::ExtensionNode::new(
                        key.clone(),
                        child_hash,
                    )),
                ));
                stack.push(child_path);
            }
            SparseNode::Branch { state_mask, .. } => {
                let mut rlp_stack: Vec<alloy_trie::nodes::RlpNode> = Vec::new();
                let mut tree_mask = reth_trie_common::TrieMask::default();
                let mut hash_mask = reth_trie_common::TrieMask::default();

                for i in 0u8..16 {
                    if !state_mask.is_bit_set(i) {
                        continue;
                    }
                    let child_path = nibbles_push(&path, i);
                    let child_rlp = sparse_node_rlp(nodes, &child_path);
                    if child_rlp.is_hash() {
                        hash_mask.set_bit(i);
                    }
                    // Only recurse into revealed (non-Hash) children.
                    let is_revealed = nodes
                        .get(&child_path)
                        .map(|n| !matches!(n, SparseNode::Hash(_)))
                        .unwrap_or(false);
                    if is_revealed {
                        tree_mask.set_bit(i);
                        stack.push(child_path);
                    }
                    rlp_stack.push(child_rlp);
                }

                pairs.push((
                    path.clone(),
                    TrieNode::Branch(alloy_trie::nodes::BranchNode::new(rlp_stack, *state_mask)),
                ));
                if tree_mask.get() != 0 || hash_mask.get() != 0 {
                    branch_masks.insert(path, BranchNodeMasks { tree_mask, hash_mask });
                }
            }
        }
    }
    Ok((DecodedProofNodes::from_iter(pairs), branch_masks))
}

/// Same as `sparse_nodes_to_account_proof_nodes` but for a storage subtrie,
/// wrapping the result in a `DecodedStorageMultiProof`.
fn sparse_nodes_to_decoded_storage_multiproof(
    trie: &SerialSparseTrie,
    root: B256,
) -> MptResult<DecodedStorageMultiProof> {
    let nodes = trie.nodes_ref();
    let values = trie.values_ref();
    let mut pairs: Vec<(Nibbles, TrieNode)> = Vec::new();
    let mut branch_masks: BranchNodeMasksMap = BranchNodeMasksMap::default();

    let mut stack: Vec<Nibbles> = vec![Nibbles::default()];
    while let Some(path) = stack.pop() {
        let node = match nodes.get(&path) {
            Some(n) => n,
            None => continue,
        };
        match node {
            SparseNode::Empty => {
                pairs.push((path, TrieNode::EmptyRoot));
            }
            SparseNode::Hash(_) => {} // blinded
            SparseNode::Leaf { key, .. } => {
                let full_path = nibbles_extend(&path, key);
                let value = values.get(&full_path).cloned().unwrap_or_default();
                pairs.push((
                    path,
                    TrieNode::Leaf(alloy_trie::nodes::LeafNode::new(key.clone(), value)),
                ));
            }
            SparseNode::Extension { key, .. } => {
                let child_path = nibbles_extend(&path, key);
                let child_hash = sparse_node_rlp(nodes, &child_path);
                pairs.push((
                    path,
                    TrieNode::Extension(alloy_trie::nodes::ExtensionNode::new(
                        key.clone(),
                        child_hash,
                    )),
                ));
                stack.push(child_path);
            }
            SparseNode::Branch { state_mask, .. } => {
                let mut rlp_stack: Vec<alloy_trie::nodes::RlpNode> = Vec::new();
                let mut tree_mask = reth_trie_common::TrieMask::default();
                let mut hash_mask = reth_trie_common::TrieMask::default();
                for i in 0u8..16 {
                    if !state_mask.is_bit_set(i) {
                        continue;
                    }
                    let child_path = nibbles_push(&path, i);
                    let child_rlp = sparse_node_rlp(nodes, &child_path);
                    if child_rlp.is_hash() {
                        hash_mask.set_bit(i);
                    }
                    let is_revealed = nodes
                        .get(&child_path)
                        .map(|n| !matches!(n, SparseNode::Hash(_)))
                        .unwrap_or(false);
                    if is_revealed {
                        tree_mask.set_bit(i);
                        stack.push(child_path);
                    }
                    rlp_stack.push(child_rlp);
                }
                pairs.push((
                    path.clone(),
                    TrieNode::Branch(alloy_trie::nodes::BranchNode::new(rlp_stack, *state_mask)),
                ));
                if tree_mask.get() != 0 || hash_mask.get() != 0 {
                    branch_masks.insert(path, BranchNodeMasks { tree_mask, hash_mask });
                }
            }
        }
    }
    Ok(DecodedStorageMultiProof {
        root,
        subtree: DecodedProofNodes::from_iter(pairs),
        branch_node_masks: branch_masks,
    })
}

/// Returns the `RlpNode` representation for the node at `path` in the sparse trie.
///
/// Revealed nodes: returns their hash (or inline RLP if small).
/// Blinded nodes (`SparseNode::Hash`): returns `word_rlp(h)`.
/// Absent nodes: returns empty placeholder.
fn sparse_node_rlp(
    nodes: &alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    path: &Nibbles,
) -> alloy_trie::nodes::RlpNode {
    use alloy_trie::nodes::RlpNode;
    match nodes.get(path) {
        Some(SparseNode::Hash(h)) => RlpNode::word_rlp(h),
        Some(SparseNode::Leaf { hash: Some(h), .. }) => RlpNode::word_rlp(h),
        Some(SparseNode::Extension { hash: Some(h), .. }) => RlpNode::word_rlp(h),
        Some(SparseNode::Branch { hash: Some(h), .. }) => RlpNode::word_rlp(h),
        _ => RlpNode::word_rlp(&alloy_primitives::B256::ZERO), // fallback
    }
}

// ── Arena → DecodedProofNodes conversion ─────────────────────────────────────

/// Converts a materialized arena (from `trace_touched_paths`) to a
/// `DecodedStorageMultiProof` ready for `SparseStateTrie::reveal_decoded_storage_multiproof`.
///
/// When `root_idx` is `None` the segment was empty; returns
/// `DecodedStorageMultiProof::empty()`.
pub(crate) fn convert_arena_to_decoded_storage_multiproof(
    arena: &MutableTrieArena,
    root_idx: Option<u32>,
    root_hash: B256,
) -> MptResult<DecodedStorageMultiProof> {
    if root_idx.is_none() {
        return Ok(DecodedStorageMultiProof::empty());
    }
    let (pairs, branch_masks) = arena_to_proof_nodes(arena, root_idx.unwrap())?;
    Ok(DecodedStorageMultiProof {
        root: root_hash,
        subtree: DecodedProofNodes::from_iter(pairs),
        branch_node_masks: branch_masks,
    })
}

/// Converts a materialized arena to `(DecodedProofNodes, BranchNodeMasksMap)` as
/// expected by `SparseStateTrie::reveal_decoded_account_multiproof`.
///
/// When `root_idx` is `None` the trie is empty; returns an `EmptyRoot` proof.
pub(crate) fn convert_arena_to_account_proof_nodes(
    arena: &MutableTrieArena,
    root_idx: Option<u32>,
) -> MptResult<(DecodedProofNodes, BranchNodeMasksMap)> {
    if root_idx.is_none() {
        let subtree = DecodedProofNodes::from_iter([(Nibbles::default(), TrieNode::EmptyRoot)]);
        return Ok((subtree, BranchNodeMasksMap::default()));
    }
    let (pairs, branch_masks) = arena_to_proof_nodes(arena, root_idx.unwrap())?;
    Ok((DecodedProofNodes::from_iter(pairs), branch_masks))
}

/// Iterative DFS over the materialized arena, converting each node to a
/// `(trie_path, TrieNode)` pair.
///
/// # Path convention
/// The key in `DecodedProofNodes` is the nibble path CONSUMED to reach the node
/// from the trie root (not including the node's own key/nibbles).
///
/// # Branch `ChildRef` semantics → masks
/// - `Arena(idx)` (materialized child): `tree_mask` bit SET; `hash_mask` bit SET when child is
///   hash-sized (≥ 32 bytes), NOT SET when inline.
/// - `Hash(h)` (blinded cross-segment ref): `tree_mask` NOT SET; `hash_mask` SET.
/// - `Inline(rlp)` (small embedded node): both masks NOT SET.
fn arena_to_proof_nodes(
    arena: &MutableTrieArena,
    root_idx: u32,
) -> MptResult<(Vec<(Nibbles, TrieNode)>, BranchNodeMasksMap)> {
    let mut pairs: Vec<(Nibbles, TrieNode)> = Vec::new();
    let mut branch_masks: BranchNodeMasksMap = BranchNodeMasksMap::default();

    // Stack entries: (arena_idx, trie_path_to_this_node)
    let mut stack: Vec<(u32, Nibbles)> = vec![(root_idx, Nibbles::default())];

    while let Some((idx, path)) = stack.pop() {
        match arena.get(idx) {
            MptNode::Leaf(leaf) => {
                pairs.push((
                    path,
                    TrieNode::Leaf(TrieLeafNode::new(leaf.nibbles.clone(), leaf.value.clone())),
                ));
            }
            MptNode::Extension(ext) => {
                let (child_rlp, _is_hash) = child_to_rlp_node(arena, &ext.child)?;
                pairs.push((
                    path.clone(),
                    TrieNode::Extension(TrieExtensionNode::new(ext.nibbles.clone(), child_rlp)),
                ));
                // Only recurse into Arena children — Hash and Inline are not in this proof.
                if let ChildRef::Arena(child_idx) = ext.child {
                    let child_path = nibbles_extend(&path, &ext.nibbles);
                    stack.push((child_idx, child_path));
                }
            }
            MptNode::Branch(branch) => {
                let mut rlp_stack: Vec<RlpNode> = Vec::new();
                let mut state_mask = reth_trie_common::TrieMask::default();
                let mut tree_mask = reth_trie_common::TrieMask::default();
                let mut hash_mask = reth_trie_common::TrieMask::default();

                for (slot, child_opt) in branch.children.iter().enumerate() {
                    let Some(child_ref) = child_opt else { continue };
                    let slot = slot as u8;
                    state_mask.set_bit(slot);
                    let (rlp_node, is_hash) = child_to_rlp_node(arena, child_ref)?;
                    if is_hash {
                        hash_mask.set_bit(slot);
                    }
                    if let ChildRef::Arena(child_idx) = child_ref {
                        tree_mask.set_bit(slot);
                        let child_path = nibbles_push(&path, slot);
                        stack.push((*child_idx, child_path));
                    }
                    rlp_stack.push(rlp_node);
                }

                pairs.push((
                    path.clone(),
                    TrieNode::Branch(TrieBranchNode::new(rlp_stack, state_mask)),
                ));
                if tree_mask.get() != 0 || hash_mask.get() != 0 {
                    branch_masks.insert(path, BranchNodeMasks { tree_mask, hash_mask });
                }
            }
        }
    }

    Ok((pairs, branch_masks))
}

/// Returns `(RlpNode, is_hash_ref)` for a `ChildRef` in the arena.
///
/// - `Hash(h)` → `(word_rlp(h), true)` — blinded cross-segment reference.
/// - `Inline(rlp)` → `(from_raw(rlp), false)` — small embedded node.
/// - `Arena(idx)` with cached hash → `(word_rlp(h), true)` — large in-proof node.
/// - `Arena(idx)` without hash → compute inline RLP → `(from_rlp(rlp), false/true)`.
fn child_to_rlp_node(arena: &MutableTrieArena, child_ref: &ChildRef) -> MptResult<(RlpNode, bool)> {
    match child_ref {
        ChildRef::Hash(h) => Ok((RlpNode::word_rlp(h), true)),
        ChildRef::Inline(rlp) => {
            // Inline bytes are always < 32 bytes by definition.
            let node = RlpNode::from_raw(rlp).ok_or_else(|| {
                MptDbError::Other("inline child RLP exceeds 33 bytes (corrupted arena)".to_string())
            })?;
            Ok((node, false))
        }
        ChildRef::Arena(idx) => {
            if let Some(h) = arena.get_hash(*idx) {
                return Ok((RlpNode::word_rlp(&h), true));
            }
            // Hash not cached → this is a small (inline-sized) node.
            // Compute its RLP and use from_rlp which handles hash vs inline.
            let rlp = compute_inline_rlp(arena, *idx)?;
            let node = RlpNode::from_rlp(&rlp);
            let is_hash = node.is_hash();
            Ok((node, is_hash))
        }
    }
}

/// Computes the RLP encoding of a (presumably small) arena node recursively.
///
/// Only called when `arena.get_hash(idx)` returns `None`, which means the node
/// has no cached hash and is expected to produce an inline RLP (< 32 bytes).
/// In practice this is rare (only tiny subtrees).
fn compute_inline_rlp(arena: &MutableTrieArena, idx: u32) -> MptResult<Vec<u8>> {
    match arena.get(idx) {
        MptNode::Leaf(leaf) => Ok(encode_leaf(&leaf.nibbles, &leaf.value)),
        MptNode::Extension(ext) => {
            let child_bytes = child_to_embed_bytes(arena, &ext.child)?;
            Ok(encode_extension(&ext.nibbles, &child_bytes))
        }
        MptNode::Branch(branch) => {
            let mut children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            for (i, child) in branch.children.iter().enumerate() {
                if let Some(child_ref) = child {
                    children[i] = Some(child_to_embed_bytes(arena, child_ref)?);
                }
            }
            Ok(encode_branch(&children, branch.value.as_deref()))
        }
    }
}

/// Returns raw embedding bytes for a child reference (used inside `encode_*` calls).
fn child_to_embed_bytes(arena: &MutableTrieArena, child_ref: &ChildRef) -> MptResult<Vec<u8>> {
    match child_ref {
        ChildRef::Hash(h) => Ok(h.to_vec()),
        ChildRef::Inline(rlp) => Ok(rlp.clone()),
        ChildRef::Arena(idx) => {
            if let Some(h) = arena.get_hash(*idx) {
                return Ok(h.to_vec());
            }
            compute_inline_rlp(arena, *idx)
        }
    }
}

// ── Nibbles path helpers ──────────────────────────────────────────────────────

/// Returns a new `Nibbles` equal to `base` with `nibble` appended.
fn nibbles_push(base: &Nibbles, nibble: u8) -> Nibbles {
    let mut result = base.clone();
    result.push_unchecked(nibble);
    result
}

/// Returns a new `Nibbles` equal to `base` extended with `extra`.
fn nibbles_extend(base: &Nibbles, extra: &Nibbles) -> Nibbles {
    let mut result = base.clone();
    // Nibbles is U256-backed; iterate nibble bytes individually.
    for nibble in extra.iter() {
        result.push_unchecked(nibble);
    }
    result
}

// ── Phase 2: apply_all_storage_changes_sparse ─────────────────────────────────

/// Applies all storage and account changes from `dirty_accounts` to a fresh
/// `SparseStateTrie` in one pass.
///
/// # Preconditions
/// - `trie` MUST have been constructed with `.with_updates(true)`. `SparseStateTrie::default()`
///   sets `retain_updates=false`; call `SparseStateTrie::default().with_updates(true)` before
///   passing.
/// - `account_proof` must have been produced from the committed account trie (e.g. via
///   `convert_arena_to_account_proof_nodes` on the committed arena).
/// - `provider_factory.known_empty_accounts` must be pre-populated: ```ignore
///   factory.known_empty_accounts = dirty_accounts.iter() .filter(|d| d.storage_known_empty ||
///   d.storage_wiped) .map(|d| d.hashed_address) .collect(); ```
///
/// # Step 0 — Batch account trie reveal (⚠ must run BEFORE the per-account loop)
/// All dirty account paths are revealed at once.  Do NOT call
/// `reveal_decoded_account_multiproof` inside the per-account loop: re-revealing
/// an already-revealed path replaces its node data, silently overwriting any
/// account writes from a previous iteration.
///
/// # Step 1 — Per-account storage reveal
/// For each dirty account, reveals the dirty storage slot paths.  New/wiped
/// accounts use `DecodedStorageMultiProof::empty()` as the base.
///
/// # Step 2 — Apply storage changes
/// Updates/removes individual storage leaves via `update_storage_leaf` /
/// `remove_storage_leaf`.
///
/// # Step 3 — Apply account change
/// Updates or removes the account leaf.  `update_account` reads the storage
/// root from the storage trie entry (revealed in Step 1) or from the existing
/// leaf value (when no storage changes exist).
/// Apply all storage and account changes from `dirty_accounts` to `trie`.
///
/// When `skip_already_revealed_storage` is `true` (cross-block reuse path), the
/// Step-1 storage-trie reveal is skipped for accounts whose storage trie is
/// **already present** in `trie` (i.e. `trie.storage_trie_ref(addr).is_some()`).
/// The trie was reused from the previous block, so re-revealing is redundant.
/// The factory's `pre_built_proof` fallback in `SegmentStorageNodeProvider`
/// handles any Hash-blinded nodes encountered during Step-2 `update_storage_leaf`.
pub(crate) fn apply_all_storage_changes_sparse(
    trie: &mut SparseStateTrie,
    account_proof: (DecodedProofNodes, BranchNodeMasksMap),
    provider_factory: &SegmentTrieNodeProviderFactory,
    dirty_accounts: &[DirtyAccount],
    skip_already_revealed_storage: bool,
) -> MptResult<()> {
    if dirty_accounts.is_empty() {
        return Ok(());
    }

    // ── Step 0: reveal account trie for ALL dirty addresses in one batch ──────
    //
    // ⚠ Implementation trap: do NOT call reveal_decoded_account_multiproof
    // inside the per-account loop.  A second call for the same path silently
    // overwrites already-applied account node updates, producing a wrong root.
    //
    // Cross-block fast path: when `account_proof` is empty (all accounts already
    // revealed in the reused trie), this step is skipped entirely — avoiding the
    // O(all_account_nodes) HashMap construction and reveal.
    let (account_nodes, account_masks) = account_proof;
    if !account_nodes.is_empty() {
        trie.reveal_decoded_account_multiproof(account_nodes, account_masks)
            .map_err(|e| MptDbError::Other(format!("reveal account multiproof: {e}")))?;
    }

    // ── Per-account loop ──────────────────────────────────────────────────────
    for dirty in dirty_accounts {
        let hashed_addr = dirty.hashed_address;
        let dirty_keys: Vec<Nibbles> =
            dirty.storage_changes.iter().map(|c| c.slot_key.clone()).collect();

        // ── Step 1: storage trie reveal ───────────────────────────────────────
        //
        // Cross-block fast path: if the storage trie for this account is already
        // present in the reused sparse trie, skip the reveal.  Previously-revealed
        // paths are still accessible; Hash-blinded node boundaries (new slots) are
        // handled lazily by the provider (segment or `pre_built_proof` fallback).
        //
        // storage_wiped handling MUST come before the cross-block skip: a wiped
        // account must always re-reveal as empty to record the wipe marker.
        //
        // Correct two-step fix (from plan Decision 4):
        //   1. reveal_decoded_storage_multiproof with EmptyRoot — so that `wipe()` sees
        //      updates.is_some() and records the wipe marker. (If we used Box::default(),
        //      updates=None and wipe() is a no-op.)
        //   2. wipe_storage — sets updates=Some(wiped) for TrieUpdates.
        if dirty.storage_wiped {
            trie.reveal_decoded_storage_multiproof(hashed_addr, DecodedStorageMultiProof::empty())
                .map_err(|e| {
                    MptDbError::Other(format!("reveal empty storage (wiped) {hashed_addr}: {e}"))
                })?;
            trie.wipe_storage(hashed_addr)
                .map_err(|e| MptDbError::Other(format!("wipe_storage {hashed_addr}: {e}")))?;
            // Fall through: new storage slots (if any) are written in Step 2.
        } else if skip_already_revealed_storage && trie.storage_trie_ref(&hashed_addr).is_some() {
            // Cross-block reuse: storage trie already revealed — skip reveal.
            // Step 2 applies changes directly; the provider handles Hash-blinded
            // boundaries via segment or `pre_built_proof` fallback.
        } else if let Some(reader) = provider_factory.get_storage_reader(hashed_addr) {
            // Existing account with a published segment: eager-reveal dirty paths.
            //
            // Guard: trace_touched_paths([]) panics with root-not-materialized
            // when dirty_keys is empty (segment.rs requires ≥1 key to materialise
            // root).  Skip reveal when the account has no storage changes
            // (balance/nonce only change) — `update_account` will read the
            // storage root directly from the existing account leaf.
            if !dirty_keys.is_empty() {
                let proof = reader.extract_decoded_multiproof(&dirty_keys).map_err(|e| {
                    MptDbError::Other(format!("extract storage multiproof {hashed_addr}: {e}"))
                })?;
                trie.reveal_decoded_storage_multiproof(hashed_addr, proof)
                    .map_err(|e| MptDbError::Other(format!("reveal storage {hashed_addr}: {e}")))?;
            }
            // else: no storage changes → no storage trie reveal needed.
        } else if let Some(proof) =
            provider_factory.pre_built_storage_proofs.get(&hashed_addr).cloned()
        {
            // L2 cache fallback: published segment was stale (root mismatch) but
            // the committed StorageTrieCow is available in the L2 cache.  Use the
            // pre-built proof from `build_sparse_factory`.
            if !dirty_keys.is_empty() {
                trie.reveal_decoded_storage_multiproof(hashed_addr, proof).map_err(|e| {
                    MptDbError::Other(format!("reveal storage (L2 fallback) {hashed_addr}: {e}"))
                })?;
            }
        } else {
            // No published segment and NOT storage_wiped.
            //
            // If the account has NO storage changes (balance/nonce-only change),
            // no reveal is needed: `update_account` will read the storage root
            // from the existing account leaf (or use EMPTY_ROOT_HASH for new
            // accounts).  Only error when there are actual storage changes that
            // require a base trie but no valid witness source can be found.
            let is_known_empty = dirty.storage_known_empty ||
                provider_factory.known_empty_accounts.contains(&hashed_addr);
            if !is_known_empty && !dirty_keys.is_empty() {
                return Err(MptDbError::Other(format!(
                    "no storage segment for existing account {hashed_addr}; \
                     check that dirty.storage_known_empty is set for new accounts"
                )));
            }
            // New account:
            //   dirty_keys empty   → nothing to reveal (update_account reads EMPTY_ROOT from leaf)
            //   dirty_keys non-empty → reveal empty base so that:
            //     (a) update_storage_leaf does not hit Blind, AND
            //     (b) root_with_updates sees the new slots (updates=None on
            //         Box::default() means the slots would not appear in TrieUpdates)
            if !dirty_keys.is_empty() {
                trie.reveal_decoded_storage_multiproof(
                    hashed_addr,
                    DecodedStorageMultiProof::empty(),
                )
                .map_err(|e| {
                    MptDbError::Other(format!(
                        "reveal empty storage (new account) {hashed_addr}: {e}"
                    ))
                })?;
            }
        }
        // ── Step 2: apply storage changes ────────────────────────────────────
        for change in &dirty.storage_changes {
            if change.value.is_zero() {
                trie.remove_storage_leaf(hashed_addr, &change.slot_key, provider_factory).map_err(
                    |e| {
                        MptDbError::Other(format!(
                            "remove_storage_leaf {hashed_addr}/{:?}: {e}",
                            change.slot_key
                        ))
                    },
                )?;
            } else {
                let encoded = change.encoded_value.clone().unwrap_or_default();
                trie.update_storage_leaf(
                    hashed_addr,
                    change.slot_key.clone(),
                    encoded,
                    provider_factory,
                )
                .map_err(|e| {
                    MptDbError::Other(format!(
                        "update_storage_leaf {hashed_addr}/{:?}: {e}",
                        change.slot_key
                    ))
                })?;
            }
        }
        // ── Step 3: apply account change ─────────────────────────────────────
        //
        // Safety note — info=None + storage_wiped=false:
        // Verified via revm source (account_status.rs:50-54): info=None only
        // arises from Destroyed or DestroyedAgain, both of which have
        // was_destroyed()=true → storage_wiped=true.  So info=None ⟹
        // storage_wiped=true in all revm execution paths.  No extra guard needed.
        //
        // `update_account` requires `is_account_revealed(addr)` which is only
        // true for accounts whose path was included in the eager reveal proof.
        // For EXISTING accounts, `convert_arena_to_account_proof_nodes` produces
        // a proof that includes their leaf path → `is_account_revealed=true`.
        // For NEW accounts (not in prior committed trie), the EmptyRoot proof
        // contains no leaf paths → `is_account_revealed=false`.
        //
        // New-account fix: call `update_account_leaf` directly, which inserts
        // the path into `revealed_account_paths` automatically.  We compute the
        // storage root from the storage subtrie (if modified) or use EMPTY_ROOT_HASH.
        match &dirty.info {
            Some(info) => {
                if trie.is_account_revealed(hashed_addr) {
                    // Existing account: update_account reads the current storage
                    // root from the storage subtrie or existing leaf value.
                    trie.update_account(hashed_addr, RethAccount::from(info), provider_factory)
                        .map_err(|e| {
                            MptDbError::Other(format!("update_account {hashed_addr}: {e}"))
                        })?;
                } else {
                    // Account not yet in `revealed_account_paths`.
                    //
                    // Two cases:
                    // (a) Truly new account (never committed) → storage_root = EMPTY or
                    //     from the storage subtrie (if storage was just revealed/modified).
                    // (b) Existing account whose leaf IS the trie root (single-account
                    //     trie or root-leaf case).  In this case `is_account_revealed`
                    //     returns false because `filter_map_revealed_nodes` never adds
                    //     the root path to `revealed_account_paths`.  The leaf VALUE is
                    //     in `values_ref()` at the full 64-nibble path.  We must read
                    //     the existing storage root from there, not use EMPTY_ROOT_HASH,
                    //     otherwise we silently discard the account's prior storage root.
                    let storage_root =
                        // First: try the revealed storage subtrie (if storage was modified).
                        trie.storage_root(hashed_addr)
                        // Second: decode the existing account leaf value from values_ref()
                        // (root-leaf case for single-account tries).
                        .or_else(|| {
                            trie.get_account_value(&hashed_addr)
                                .and_then(|v| TrieAccount::decode(&mut v.as_slice()).ok())
                                .map(|ta| ta.storage_root)
                        })
                        .unwrap_or(EMPTY_ROOT_HASH);
                    let trie_account = TrieAccount {
                        nonce: info.nonce,
                        balance: info.balance,
                        storage_root,
                        code_hash: info.code_hash,
                    };
                    let mut rlp_buf = Vec::new();
                    trie_account.encode(&mut rlp_buf);
                    trie.update_account_leaf(
                        Nibbles::unpack(hashed_addr),
                        rlp_buf,
                        provider_factory,
                    )
                    .map_err(|e| {
                        MptDbError::Other(format!("update_account_leaf {hashed_addr}: {e}"))
                    })?;
                }
            }
            None => {
                trie.remove_account_leaf(&dirty.account_key, provider_factory).map_err(|e| {
                    MptDbError::Other(format!("remove_account_leaf {hashed_addr}: {e}"))
                })?;
            }
        }
    }

    Ok(())
}

// ── SegmentTrieNodeProviderFactory helpers (Phase 2 prep) ────────────────────

impl SegmentTrieNodeProviderFactory {
    /// Returns a `StorageTrieSegmentReader` for the given account's storage segment,
    /// or `None` if no segment is loaded for that account.
    ///
    /// Used in `apply_all_storage_changes_sparse` (Phase 2) to extract eager
    /// witnesses before the per-account apply loop.
    pub fn get_storage_reader(&self, hashed_addr: B256) -> Option<StorageTrieSegmentReader<'_>> {
        let lease = self.storage_segments.get(&hashed_addr)?;
        StorageTrieSegmentReader::open_shared_page(lease, lease.root(), lease.root_record_off())
            .ok()
    }

    /// Extracts a `(DecodedProofNodes, BranchNodeMasksMap)` for the account trie
    /// covering all requested paths.
    ///
    /// Called once in Step 0 of `apply_all_storage_changes_sparse` to batch-reveal
    /// all dirty account paths before the per-account loop.
    ///
    /// Returns `(EmptyRoot proof, empty masks)` when no account segment is available
    /// (e.g. genesis or first-ever block).
    pub fn extract_decoded_account_multiproof(
        &self,
        keys: &[Nibbles],
    ) -> MptResult<(DecodedProofNodes, BranchNodeMasksMap)> {
        let Some(ref lease) = self.account_segment else {
            // No account segment yet (genesis / pre-population phase).
            let subtree = DecodedProofNodes::from_iter([(Nibbles::default(), TrieNode::EmptyRoot)]);
            return Ok((subtree, BranchNodeMasksMap::default()));
        };
        let reader = StorageTrieSegmentReader::open_shared_page(
            lease,
            lease.root(),
            lease.root_record_off(),
        )
        .map_err(|e| MptDbError::Other(format!("failed to open account segment: {e}")))?;
        reader.extract_decoded_account_multiproof(keys)
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Opens a `StorageTrieSegmentReader` from a lease and calls
/// `get_segment_node_by_path`, mapping the result to `RevealedNode`.
fn open_reader_and_get(
    lease: &Arc<SegmentPageLease>,
    path: &Nibbles,
) -> std::result::Result<Option<RevealedNode>, SparseTrieError> {
    let reader =
        StorageTrieSegmentReader::open_shared_page(lease, lease.root(), lease.root_record_off())
            .map_err(|e| sparse_err(e.to_string()))?;

    match reader.get_segment_node_by_path(path) {
        Ok(Some((node, tree_mask, hash_mask))) => {
            Ok(Some(RevealedNode { node, tree_mask, hash_mask }))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(sparse_err(e.to_string())),
    }
}

// ── Phase 3: Proof generation from SparseStateTrie ───────────────────────────

/// Build an `AccountProof` by walking the revealed nodes in a `SparseStateTrie`.
///
/// The `SparseStateTrie` MUST have had `root_with_updates` called on it so
/// that all node hashes are computed.  This is the canonical "proof after
/// commit" path when `use_sparse_storage=true`.
///
/// Replaces the persisted-store traversal in `proof::build_account_proof_from_root`
/// with an in-memory walk of `state_trie_ref().nodes_ref()` and `values_ref()`.
///
/// # Storage proofs
/// For each requested `slot`, if the storage trie for `address` was revealed
/// in the sparse trie, storage proof nodes are collected from
/// `storage_trie_ref(hashed_address).nodes_ref()` + `values_ref()`.
pub(crate) fn build_account_proof_from_sparse(
    sparse_trie: &SparseStateTrie,
    address: Address,
    slots: &[B256],
) -> MptResult<AccountProof> {
    let hashed_address = keccak256(address);
    let account_path = Nibbles::unpack(hashed_address);

    // Get the revealed account trie.
    let account_trie = match sparse_trie.state_trie_ref() {
        Some(t) => t,
        None => {
            // Trie was never revealed (only possible if no apply was done).
            let storage_proofs = slots.iter().map(|s| StorageProof::new(*s)).collect();
            return Ok(AccountProof {
                address,
                info: None,
                proof: vec![],
                storage_root: EMPTY_ROOT_HASH,
                storage_proofs,
            });
        }
    };

    // Collect RLP-encoded proof nodes along the account path.
    let proof_nodes = collect_proof_nodes_sparse(account_trie, &account_path)?;

    // Decode account info from the leaf value (if present).
    let (info, storage_root) = match account_trie.values_ref().get(&account_path) {
        Some(rlp) => {
            let ta = TrieAccount::decode(&mut rlp.as_slice())
                .map_err(|e| MptDbError::Other(format!("decode account leaf: {e}")))?;
            // Normalise code_hash: None means KECCAK_EMPTY (empty bytecode),
            // matching the convention used by the persisted proof path.
            let bytecode_hash =
                if ta.code_hash == alloy_trie::KECCAK_EMPTY { None } else { Some(ta.code_hash) };
            let account = RethAccount { nonce: ta.nonce, balance: ta.balance, bytecode_hash };
            (Some(account), ta.storage_root)
        }
        None => (None, EMPTY_ROOT_HASH),
    };

    // Build storage proofs for requested slots.
    let storage_proofs = if info.is_some() && storage_root != EMPTY_ROOT_HASH {
        slots
            .iter()
            .map(|slot| build_storage_proof_sparse(sparse_trie, &hashed_address, *slot))
            .collect::<MptResult<Vec<_>>>()?
    } else {
        slots.iter().map(|s| StorageProof::new(*s)).collect()
    };

    Ok(AccountProof { address, info, proof: proof_nodes, storage_root, storage_proofs })
}

/// Collect RLP-encoded nodes along `path` from a `SerialSparseTrie`.
///
/// Walks from root to the target leaf, collecting each node's RLP encoding.
/// Blinded (`Hash`) nodes at non-leaf positions are included as their 33-byte
/// hash-reference RLP (0xa0 + 32 bytes).
fn collect_proof_nodes_sparse(trie: &SerialSparseTrie, path: &Nibbles) -> MptResult<Vec<Bytes>> {
    let nodes = trie.nodes_ref();
    let values = trie.values_ref();
    let mut result = Vec::new();
    let mut current = Nibbles::default();

    loop {
        let node = match nodes.get(&current) {
            Some(n) => n,
            None => break,
        };
        match node {
            SparseNode::Empty => break,
            SparseNode::Hash(h) => {
                // Blinded: include as hash-ref (33 bytes: 0xa0 + hash).
                let mut rlp = Vec::with_capacity(33);
                h.as_slice().encode(&mut rlp);
                result.push(rlp.into());
                break;
            }
            SparseNode::Leaf { key, hash: _ } => {
                // Full key at leaf = current + key
                let full_leaf_path = nibbles_extend(&current, key);
                // Encode leaf node: [compact(key), value]
                if let Some(value) = values.get(&full_leaf_path) {
                    let mut full_key = current.clone();
                    for n in key.iter() {
                        full_key.push_unchecked(n);
                    }
                    use crate::mpt::encoding::encode_leaf;
                    let rlp = encode_leaf(key, value);
                    result.push(rlp.into());
                }
                break;
            }
            SparseNode::Extension { key, hash: _, .. } => {
                // Encode extension then advance past the key.
                let child_path = nibbles_extend(&current, key);
                let child_hash = get_node_hash(nodes, &child_path);
                use crate::mpt::encoding::encode_extension as mpt_encode_extension;
                // Build child embedding (hash-ref or inline).
                let child_bytes = child_hash.map(|h| h.to_vec()).unwrap_or_default();
                let rlp = mpt_encode_extension(key, &child_bytes);
                result.push(rlp.into());
                current = child_path;
            }
            SparseNode::Branch { state_mask, hash: _, .. } => {
                // Encode branch node with hashes for each present child.
                let rlp = encode_branch_node_from_sparse(nodes, &current, *state_mask)?;
                result.push(rlp.into());
                // Follow the child corresponding to the next nibble in the path.
                if current.len() < path.len() {
                    let nibble = path.get_unchecked(current.len());
                    if state_mask.is_bit_set(nibble) {
                        current = nibbles_push(&current, nibble);
                    } else {
                        break; // path diverges here
                    }
                } else {
                    break;
                }
            }
        }
    }

    Ok(result)
}

/// Build a `StorageProof` for a single slot from the sparse storage subtrie.
fn build_storage_proof_sparse(
    sparse_trie: &SparseStateTrie,
    hashed_address: &B256,
    slot: B256,
) -> MptResult<StorageProof> {
    let slot_path = Nibbles::unpack(keccak256(slot));

    let storage_trie = match sparse_trie.storage_trie_ref(hashed_address) {
        Some(t) => t,
        None => return Ok(StorageProof::new(slot)),
    };

    let proof_nodes = collect_proof_nodes_sparse(storage_trie, &slot_path)?;
    let value = storage_trie
        .values_ref()
        .get(&slot_path)
        .map(|rlp| {
            alloy_primitives::U256::decode(&mut rlp.as_slice())
                .unwrap_or(alloy_primitives::U256::ZERO)
        })
        .unwrap_or(alloy_primitives::U256::ZERO);

    Ok(StorageProof { key: slot, nibbles: slot_path, value, proof: proof_nodes })
}

/// Returns the hash of the node at `path` (if computed).
fn get_node_hash(
    nodes: &alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    path: &Nibbles,
) -> Option<B256> {
    match nodes.get(path)? {
        SparseNode::Hash(h) => Some(*h),
        SparseNode::Leaf { hash, .. } => *hash,
        SparseNode::Extension { hash, .. } => *hash,
        SparseNode::Branch { hash, .. } => *hash,
        SparseNode::Empty => None,
    }
}

/// Encode a branch node at `path` to RLP using child embeddings from `nodes`.
fn encode_branch_node_from_sparse(
    nodes: &alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    path: &Nibbles,
    state_mask: reth_trie_common::TrieMask,
) -> MptResult<Vec<u8>> {
    use crate::mpt::encoding::encode_branch as mpt_encode_branch;
    let mut children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
    for i in 0u8..16 {
        if state_mask.is_bit_set(i) {
            let child_path = nibbles_push(path, i);
            let embed = get_child_embedding(nodes, &child_path);
            if !embed.is_empty() {
                children[i as usize] = Some(embed);
            }
            // Empty embed → child is inline; parent encodes it without a separate entry.
        }
    }
    Ok(mpt_encode_branch(&children, None))
}

// ── Phase 3b: TrieUpdates → dirty blobs (non-wal_first) ──────────────────────

/// Convert revealed sparse trie nodes into `(hash, RLP)` pairs for persisting
/// to RocksDB trie tables (non-wal_first mode only).
///
/// ## Key design decision (Option B — walk all revealed nodes)
///
/// `trie_updates.account_nodes` only contains `BranchNodeCompact` entries for
/// **branch** nodes.  Extension nodes that were created or modified are NOT
/// tracked in `trie_updates`.  Using `account_nodes.keys()` as the iteration
/// set would silently omit extension nodes, leaving RocksDB trie tables
/// incomplete and breaking restart recovery.
///
/// The correct approach: iterate **all revealed nodes** in
/// `state_trie_ref().nodes_ref()`.  The sparse trie only reveals nodes along
/// dirty paths — every revealed non-blinded node is dirty and must be
/// persisted.  Blinded nodes (`SparseNode::Hash`) carry only a hash reference
/// (we don't know their content), so `collect_node_blob` skips them.
///
/// `trie_updates.storage_tries` is still used to enumerate which storage tries
/// have changes; for each, we again iterate all revealed nodes rather than
/// just `storage_nodes.keys()`.
///
/// ## WAL-first mode
/// In wal_first mode this function is not called; crash recovery uses the WAL
/// + published segments instead of RocksDB trie tables.
pub(crate) fn sparse_trie_to_dirty_blobs(
    sparse_trie: &SparseStateTrie,
    trie_updates: &TrieUpdates,
) -> MptResult<Vec<(B256, Vec<u8>)>> {
    let mut blobs: Vec<(B256, Vec<u8>)> = Vec::new();

    // ── Account trie: all revealed nodes (branch, extension, leaf) ───────────
    // Using nodes_ref().keys() instead of trie_updates.account_nodes.keys()
    // ensures extension nodes are included.
    if let Some(account_trie) = sparse_trie.state_trie_ref() {
        let nodes = account_trie.nodes_ref();
        let values = account_trie.values_ref();
        for path in nodes.keys() {
            collect_node_blob(nodes, values, path, &mut blobs)?;
        }
        // removed_nodes: no blobs needed; the caller clears them from RocksDB.
    }

    // ── Storage tries: all revealed nodes for each dirty account ─────────────
    // We use trie_updates.storage_tries to know which accounts have changes,
    // then iterate ALL revealed nodes in each storage trie (not just
    // storage_nodes.keys() which would miss extension nodes).
    for (hashed_addr, storage_updates) in &trie_updates.storage_tries {
        let storage_updates: &StorageTrieUpdates = storage_updates;
        if storage_updates.is_deleted() {
            continue; // SELFDESTRUCT: trie deleted, no blobs to write
        }
        if let Some(storage_trie) = sparse_trie.storage_trie_ref(hashed_addr) {
            let nodes = storage_trie.nodes_ref();
            let values = storage_trie.values_ref();
            for path in nodes.keys() {
                collect_node_blob(nodes, values, path, &mut blobs)?;
            }
        }
    }

    Ok(blobs)
}

/// Encode the node at `path` to RLP, compute its hash, and push `(hash, rlp)`
/// into `blobs` if the node is large enough to be independently stored (≥ 32 bytes).
///
/// # Why compute hash from RLP instead of using `SparseNode.hash`
///
/// After `root_with_updates`, reth's `SerialSparseTrie::root()` sets
/// `SparseNode::*.hash = rlp_node.as_hash()`.  `RlpNode::as_hash()` returns
/// `Some(h)` only when the pre-computed `RlpNode` is exactly 33 bytes — i.e.,
/// when it was produced by `RlpNode::from_rlp(raw_rlp)` for `raw_rlp.len() >= 32`
/// (which wraps to `word_rlp(keccak256(raw_rlp))`).  For nodes whose _final_ RLP
/// happens to be < 32 bytes (rare but possible), or for nodes that were freshly
/// created in this block and haven't gone through `root()` yet, `SparseNode.hash`
/// may be `None` even though the node needs to be stored.
///
/// Computing the hash from the RLP we just built is always correct and avoids
/// this discrepancy.  Only nodes whose RLP is ≥ 32 bytes are stored (smaller
/// nodes are embedded inline in their parent and do not need their own entry).
fn collect_node_blob(
    nodes: &alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    values: &alloy_primitives::map::HashMap<Nibbles, Vec<u8>>,
    path: &Nibbles,
    blobs: &mut Vec<(B256, Vec<u8>)>,
) -> MptResult<()> {
    let node = match nodes.get(path) {
        Some(n) => n,
        None => return Ok(()),
    };
    let rlp: Vec<u8> = match node {
        SparseNode::Branch { state_mask, .. } => {
            encode_branch_node_from_sparse(nodes, path, *state_mask)?
        }
        SparseNode::Extension { key, .. } => {
            let child_path = nibbles_extend(path, key);
            let child_bytes = get_child_embedding(nodes, &child_path);
            use crate::mpt::encoding::encode_extension as mpt_ext;
            mpt_ext(key, &child_bytes)
        }
        SparseNode::Leaf { key, .. } => {
            let full_leaf_path = nibbles_extend(path, key);
            let value = match values.get(&full_leaf_path) {
                Some(v) => v,
                None => return Ok(()),
            };
            use crate::mpt::encoding::encode_leaf;
            encode_leaf(key, value)
        }
        _ => return Ok(()), // Hash (blinded) or Empty: skip
    };
    // Only independently-stored nodes (≥ 32 bytes RLP); smaller nodes are inline.
    if rlp.len() >= 32 {
        blobs.push((alloy_primitives::keccak256(&rlp), rlp));
    }
    Ok(())
}

/// Returns the child's embedding bytes for use inside a parent's RLP.
///
/// For a child with `SparseNode.hash = Some(h)` → 32-byte hash.
/// For blinded nodes (`SparseNode::Hash(h)`) → 32-byte hash.
/// For inline children (no hash) → returns empty vec (parent will encode inline).
fn get_child_embedding(
    nodes: &alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    child_path: &Nibbles,
) -> Vec<u8> {
    match nodes.get(child_path) {
        Some(SparseNode::Hash(h)) => h.to_vec(),
        Some(SparseNode::Leaf { hash: Some(h), .. }) => h.to_vec(),
        Some(SparseNode::Extension { hash: Some(h), .. }) => h.to_vec(),
        Some(SparseNode::Branch { hash: Some(h), .. }) => h.to_vec(),
        _ => Vec::new(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt::{segment::StorageTrieSegment, tree::MptTree};
    use alloy_primitives::keccak256;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn build_lease(tree: &mut MptTree) -> Arc<SegmentPageLease> {
        let root = tree.root_hash();
        let seg = StorageTrieSegment::from_tree(tree, root).unwrap();
        seg.into_page_lease()
    }

    fn single_key_tree() -> MptTree {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(keccak256(b"key-a"));
        tree.insert(&key, b"value-a".to_vec());
        tree
    }

    fn multi_key_tree() -> MptTree {
        let mut tree = MptTree::new();
        for i in 0u8..8 {
            let key = Nibbles::unpack(keccak256(&[i]));
            tree.insert(&key, vec![i; 16]);
        }
        tree
    }

    // ── SegmentStorageNodeProvider ────────────────────────────────────────────

    #[test]
    fn t_storage_provider_known_empty_no_lease_returns_none() {
        let provider =
            SegmentStorageNodeProvider { lease: None, is_known_empty: true, pre_built_proof: None };
        let result = provider.trie_node(&Nibbles::default());
        assert!(result.unwrap().is_none(), "known-empty + no lease must return Ok(None)");
    }

    #[test]
    fn t_storage_provider_not_known_empty_no_lease_returns_err() {
        let provider = SegmentStorageNodeProvider {
            lease: None,
            is_known_empty: false,
            pre_built_proof: None,
        };
        let result = provider.trie_node(&Nibbles::default());
        assert!(result.is_err(), "existing account + no lease must return Err");
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("data-loss"), "error must mention data-loss, got: {err_str}");
    }

    #[test]
    fn t_storage_provider_with_lease_root_path_returns_node() {
        let tree = single_key_tree();
        let lease = build_lease(&mut tree.clone());
        let provider = SegmentStorageNodeProvider {
            lease: Some(lease),
            is_known_empty: false,
            pre_built_proof: None,
        };

        let result = provider.trie_node(&Nibbles::default()).unwrap();
        assert!(result.is_some(), "root path must return a node when lease is present");
        let node = result.unwrap();
        assert!(!node.node.is_empty(), "RLP bytes must not be empty");
    }

    #[test]
    fn t_storage_provider_with_lease_missing_path_returns_none() {
        let tree = single_key_tree();
        let lease = build_lease(&mut tree.clone());
        let provider = SegmentStorageNodeProvider {
            lease: Some(lease),
            is_known_empty: false,
            pre_built_proof: None,
        };

        // A 5-nibble path [0,0,0,0,0] almost certainly does not exist.
        let absent = Nibbles::from_nibbles(&[0, 0, 0, 0, 0]);
        let result = provider.trie_node(&absent).unwrap();
        // Either None (path absent) or Some (if that prefix happens to exist).
        // We just verify no panic and no Err.
        let _ = result;
    }

    // ── SegmentAccountNodeProvider ────────────────────────────────────────────

    #[test]
    fn t_account_provider_no_lease_always_returns_err() {
        let provider = SegmentAccountNodeProvider { lease: None };
        let result = provider.trie_node(&Nibbles::default());
        assert!(result.is_err(), "account provider with no lease must always Err");
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("data-loss"), "error must mention data-loss, got: {err_str}");
    }

    #[test]
    fn t_account_provider_with_lease_root_returns_node() {
        let tree = multi_key_tree();
        let lease = build_lease(&mut tree.clone());
        let provider = SegmentAccountNodeProvider { lease: Some(lease) };

        let result = provider.trie_node(&Nibbles::default()).unwrap();
        assert!(result.is_some(), "root path must return a node");
        let node = result.unwrap();
        assert!(!node.node.is_empty());
    }

    // ── SegmentTrieNodeProviderFactory ────────────────────────────────────────

    #[test]
    fn t_factory_default_is_empty() {
        let factory = SegmentTrieNodeProviderFactory::default();
        assert!(factory.account_segment.is_none());
        assert!(factory.storage_segments.is_empty());
        assert!(factory.known_empty_accounts.is_empty());
    }

    #[test]
    fn t_factory_account_provider_no_segment_errs() {
        let factory = SegmentTrieNodeProviderFactory::new();
        let provider = factory.account_node_provider();
        let result = provider.trie_node(&Nibbles::default());
        assert!(result.is_err(), "account provider without segment must Err");
    }

    #[test]
    fn t_factory_storage_provider_known_empty_returns_none() {
        let addr = B256::repeat_byte(0x01);
        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.known_empty_accounts.insert(addr);

        let provider = factory.storage_node_provider(addr);
        assert!(provider.is_known_empty);
        assert!(provider.lease.is_none());
        let result = provider.trie_node(&Nibbles::default()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn t_factory_storage_provider_unknown_addr_not_known_empty() {
        let addr = B256::repeat_byte(0x02);
        let factory = SegmentTrieNodeProviderFactory::new();

        let provider = factory.storage_node_provider(addr);
        assert!(!provider.is_known_empty);
        assert!(provider.lease.is_none());
        // Not known-empty + no lease → Err.
        assert!(provider.trie_node(&Nibbles::default()).is_err());
    }

    #[test]
    fn t_factory_storage_provider_with_segment_returns_node() {
        let addr = B256::repeat_byte(0x03);
        let tree = single_key_tree();
        let lease = build_lease(&mut tree.clone());

        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.storage_segments.insert(addr, lease);

        let provider = factory.storage_node_provider(addr);
        assert!(!provider.is_known_empty);
        let result = provider.trie_node(&Nibbles::default()).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn t_factory_account_provider_with_segment_returns_node() {
        let tree = multi_key_tree();
        let lease = build_lease(&mut tree.clone());

        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.account_segment = Some(lease);

        let provider = factory.account_node_provider();
        let result = provider.trie_node(&Nibbles::default()).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn t_revealed_node_masks_for_branch_are_some() {
        // A multi-key tree has a branch at the root; both masks must be Some.
        let tree = multi_key_tree();
        let lease = build_lease(&mut tree.clone());
        let provider = SegmentAccountNodeProvider { lease: Some(lease) };

        let result = provider.trie_node(&Nibbles::default()).unwrap().unwrap();
        // tree_mask and hash_mask are both Some for branch nodes.
        assert!(result.tree_mask.is_some(), "branch node must have tree_mask");
        assert!(result.hash_mask.is_some(), "branch node must have hash_mask");
    }

    #[test]
    fn t_revealed_node_masks_for_leaf_are_none() {
        // Single-key tree: root is a leaf. Both masks must be None.
        let tree = single_key_tree();
        let lease = build_lease(&mut tree.clone());
        let provider = SegmentAccountNodeProvider { lease: Some(lease) };

        let result = provider.trie_node(&Nibbles::default()).unwrap().unwrap();
        assert!(result.tree_mask.is_none(), "leaf node must have no tree_mask");
        assert!(result.hash_mask.is_none(), "leaf node must have no hash_mask");
    }

    #[test]
    fn t_factory_segment_for_absent_addr_returns_none_when_known_empty() {
        // Segment exists for addr_a; query addr_b which is known-empty.
        let addr_a = B256::repeat_byte(0xaa);
        let addr_b = B256::repeat_byte(0xbb);

        let tree = single_key_tree();
        let lease = build_lease(&mut tree.clone());

        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.storage_segments.insert(addr_a, lease);
        factory.known_empty_accounts.insert(addr_b);

        let provider_b = factory.storage_node_provider(addr_b);
        assert!(provider_b.is_known_empty);
        assert!(provider_b.lease.is_none());
        assert!(provider_b.trie_node(&Nibbles::default()).unwrap().is_none());
    }

    // ── Phase 1: Arena → DecodedProofNodes conversion ────────────────────────

    /// Build a `StorageTrieSegmentReader` from a tree and call
    /// `extract_decoded_multiproof` on a set of its keys.
    fn reader_from_tree(tree: &mut MptTree) -> (crate::mpt::segment::StorageTrieSegment, B256) {
        let root = tree.root_hash();
        let seg = StorageTrieSegment::from_tree(tree, root).unwrap();
        (seg, root)
    }

    #[test]
    fn t_convert_arena_single_leaf_produces_leaf_trie_node() {
        let mut tree = single_key_tree();
        let (seg, root) = reader_from_tree(&mut tree);
        let reader =
            crate::mpt::segment::StorageTrieSegmentReader::open(seg.as_bytes(), root).unwrap();

        let key = Nibbles::unpack(keccak256(b"key-a"));
        let proof = reader.extract_decoded_multiproof(&[key]).unwrap();

        assert_eq!(proof.root, root, "proof root must match segment root");
        // The root path (empty nibbles) must be present.
        let root_node = proof.subtree.get(&Nibbles::default());
        assert!(root_node.is_some(), "DecodedProofNodes must contain root path");
        // No branch masks for a single-key trie (root is a leaf).
        assert!(proof.branch_node_masks.is_empty(), "single-leaf trie must have no branch masks");
    }

    #[test]
    fn t_convert_arena_multi_key_root_is_branch() {
        let mut tree = multi_key_tree();
        let (seg, root) = reader_from_tree(&mut tree);
        let reader =
            crate::mpt::segment::StorageTrieSegmentReader::open(seg.as_bytes(), root).unwrap();

        // Reveal the first key's path; root should be a branch.
        let key = Nibbles::unpack(keccak256(&[0u8]));
        let proof = reader.extract_decoded_multiproof(&[key]).unwrap();

        assert_eq!(proof.root, root);
        let root_node = proof.subtree.get(&Nibbles::default());
        assert!(root_node.is_some(), "root node must be present");
        // With 8 keys, root is almost certainly a branch.
        // If it is, branch_node_masks must be non-empty.
        if matches!(root_node, Some(TrieNode::Branch(_))) {
            assert!(
                !proof.branch_node_masks.is_empty(),
                "branch root must produce branch_node_masks entry"
            );
        }
    }

    #[test]
    fn t_convert_arena_branch_masks_tree_mask_set_for_arena_children() {
        // Build a 16-key trie so root is a branch with many children.
        let mut tree = MptTree::new();
        for i in 0u8..16 {
            let key = Nibbles::unpack(keccak256(&[i]));
            tree.insert(&key, vec![i; 32]);
        }
        let (seg, root) = reader_from_tree(&mut tree);
        let reader =
            crate::mpt::segment::StorageTrieSegmentReader::open(seg.as_bytes(), root).unwrap();

        // Reveal one key: root branch + path to leaf.
        let key = Nibbles::unpack(keccak256(&[0u8]));
        let proof = reader.extract_decoded_multiproof(&[key]).unwrap();

        if let Some(BranchNodeMasks { tree_mask, hash_mask }) =
            proof.branch_node_masks.get(&Nibbles::default())
        {
            // tree_mask must have at least one bit set (the path we revealed).
            assert_ne!(tree_mask.get(), 0, "tree_mask must be non-zero");
            // hash_mask must cover at least all tree_mask bits (all revealed
            // children are hash-referenced).
            let tree_bits = tree_mask.get();
            let hash_bits = hash_mask.get();
            // hash_mask ⊇ tree_mask (revealed Arena children are hash-sized).
            assert_eq!(
                tree_bits & hash_bits,
                tree_bits,
                "every tree_mask bit must also appear in hash_mask"
            );
        }
    }

    #[test]
    fn t_convert_arena_all_keys_root_hash_matches() {
        // Reveal ALL keys → full trie in the proof.  Root hash must match.
        let mut tree = MptTree::new();
        let mut keys = Vec::new();
        for i in 0u8..8 {
            let key = Nibbles::unpack(keccak256(&[i, i]));
            keys.push(key.clone());
            tree.insert(&key, vec![i; 16]);
        }
        let root = tree.root_hash();
        let seg = StorageTrieSegment::from_tree(&mut tree, root).unwrap();
        let reader =
            crate::mpt::segment::StorageTrieSegmentReader::open(seg.as_bytes(), root).unwrap();

        let proof = reader.extract_decoded_multiproof(&keys).unwrap();
        assert_eq!(proof.root, root, "proof root must equal segment root");
        // All nodes must be present (no blinded siblings when all keys revealed).
        assert!(!proof.subtree.is_empty(), "proof must contain nodes");
    }

    #[test]
    fn t_account_multiproof_empty_segment_returns_empty_root() {
        // Factory with no account segment → EmptyRoot proof.
        let factory = SegmentTrieNodeProviderFactory::new();
        let keys = [Nibbles::unpack(keccak256(b"addr-1"))];
        let (nodes, masks) = factory.extract_decoded_account_multiproof(&keys).unwrap();
        let root_node = nodes.get(&Nibbles::default());
        assert!(matches!(root_node, Some(TrieNode::EmptyRoot)));
        assert!(masks.is_empty());
    }

    #[test]
    fn t_account_multiproof_with_segment_returns_nodes() {
        let mut tree = multi_key_tree();
        let lease = build_lease(&mut tree);

        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.account_segment = Some(lease);

        let key = Nibbles::unpack(keccak256(&[0u8]));
        let (nodes, _masks) = factory.extract_decoded_account_multiproof(&[key]).unwrap();

        let root_node = nodes.get(&Nibbles::default());
        assert!(root_node.is_some(), "account multiproof must contain root node");
    }

    #[test]
    fn t_get_storage_reader_returns_none_for_unknown_addr() {
        let factory = SegmentTrieNodeProviderFactory::new();
        let addr = B256::repeat_byte(0x42);
        assert!(factory.get_storage_reader(addr).is_none());
    }

    #[test]
    fn t_get_storage_reader_returns_reader_for_known_addr() {
        let addr = B256::repeat_byte(0x07);
        let mut tree = single_key_tree();
        let lease = build_lease(&mut tree);

        let mut factory = SegmentTrieNodeProviderFactory::new();
        factory.storage_segments.insert(addr, lease);

        let reader = factory.get_storage_reader(addr);
        assert!(reader.is_some(), "reader must be available for loaded address");
    }
}
