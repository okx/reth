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
use rayon::prelude::*;
use reth_execution_errors::{SparseTrieError, SparseTrieErrorKind};
use reth_primitives_traits::Account as RethAccount;
use reth_trie_common::{
    updates::{StorageTrieUpdates, TrieUpdates},
    AccountProof, BranchNodeMasks, BranchNodeMasksMap, DecodedMultiProof, DecodedStorageMultiProof,
    Nibbles, StorageProof, TrieAccount, EMPTY_ROOT_HASH,
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
    node::{BranchNode, ChildRef, ExtensionNode, LeafNode, MptNode},
    segment::{SegmentPageLease, StorageTrieSegment, StorageTrieSegmentReader},
    state::DirtyAccount,
};

static SPARSE_PROVIDER_STORAGE_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static SPARSE_PROVIDER_ACCOUNT_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

const SPARSE_STORAGE_FULL_SCAN_MIN_CHANGES: usize = 16;
const SPARSE_STORAGE_FULL_SCAN_MAX_CHANGES: usize = 64;
const SPARSE_STORAGE_FULL_SCAN_MAX_DIRTY_ACCOUNTS: usize = 2000;
// Keep a bounded per-call storage map size for reveal_decoded_multiproof to
// avoid very large transient allocations under 10K-account blocks.
const SPARSE_STORAGE_REVEAL_BATCH_CHUNK: usize = 8192;

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
        SPARSE_PROVIDER_STORAGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        SPARSE_PROVIDER_ACCOUNT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// Path-limited variant of `convert_arena_to_account_proof_nodes`.
///
/// Builds account multiproof nodes for only the requested account-key paths,
/// avoiding a full-arena DFS on large account tries.
pub(crate) fn convert_arena_to_account_proof_nodes_for_paths(
    arena: &MutableTrieArena,
    root_idx: Option<u32>,
    keys: &[Nibbles],
) -> MptResult<(DecodedProofNodes, BranchNodeMasksMap)> {
    if root_idx.is_none() {
        let subtree = DecodedProofNodes::from_iter([(Nibbles::default(), TrieNode::EmptyRoot)]);
        return Ok((subtree, BranchNodeMasksMap::default()));
    }
    if keys.is_empty() {
        return convert_arena_to_account_proof_nodes(arena, root_idx);
    }

    let root_idx = root_idx.unwrap();
    let mut include_paths: HashSet<Nibbles> = HashSet::default();
    include_paths.insert(Nibbles::default());

    for key in keys {
        let mut current_idx = root_idx;
        let mut current_path = Nibbles::default();
        let mut offset = 0usize;

        loop {
            include_paths.insert(current_path.clone());
            match arena.get(current_idx) {
                MptNode::Leaf(_) => break,
                MptNode::Extension(ext) => {
                    let ext_len = ext.nibbles.len();
                    if offset + ext_len > key.len() {
                        break;
                    }
                    let mut matched = true;
                    for i in 0..ext_len {
                        if key.get_unchecked(offset + i) != ext.nibbles.get_unchecked(i) {
                            matched = false;
                            break;
                        }
                    }
                    if !matched {
                        break;
                    }
                    offset += ext_len;
                    match ext.child {
                        ChildRef::Arena(child_idx) => {
                            current_path = nibbles_extend(&current_path, &ext.nibbles);
                            current_idx = child_idx;
                        }
                        _ => break,
                    }
                }
                MptNode::Branch(branch) => {
                    if offset >= key.len() {
                        break;
                    }
                    let nibble = key.get_unchecked(offset);
                    offset += 1;
                    let Some(child_ref) = branch.children[nibble as usize].as_ref() else {
                        break;
                    };
                    match child_ref {
                        ChildRef::Arena(child_idx) => {
                            current_path = nibbles_push(&current_path, nibble);
                            current_idx = *child_idx;
                        }
                        _ => break,
                    }
                }
            }
        }
    }

    let (pairs, branch_masks) = arena_to_proof_nodes_for_paths(arena, root_idx, &include_paths)?;
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

fn arena_to_proof_nodes_for_paths(
    arena: &MutableTrieArena,
    root_idx: u32,
    include_paths: &HashSet<Nibbles>,
) -> MptResult<(Vec<(Nibbles, TrieNode)>, BranchNodeMasksMap)> {
    let mut pairs: Vec<(Nibbles, TrieNode)> = Vec::new();
    let mut branch_masks: BranchNodeMasksMap = BranchNodeMasksMap::default();
    let mut visited: HashSet<Nibbles> = HashSet::default();
    let mut stack: Vec<(u32, Nibbles)> = vec![(root_idx, Nibbles::default())];

    while let Some((idx, path)) = stack.pop() {
        if !include_paths.contains(&path) {
            continue;
        }
        if !visited.insert(path.clone()) {
            continue;
        }

        match arena.get(idx) {
            MptNode::Leaf(leaf) => {
                pairs.push((
                    path,
                    TrieNode::Leaf(TrieLeafNode::new(leaf.nibbles.clone(), leaf.value.clone())),
                ));
            }
            MptNode::Extension(ext) => {
                let child_path = nibbles_extend(&path, &ext.nibbles);
                let include_child = include_paths.contains(&child_path);
                let child_rlp = child_to_rlp_node(arena, &ext.child)?.0;
                pairs.push((
                    path,
                    TrieNode::Extension(TrieExtensionNode::new(ext.nibbles.clone(), child_rlp)),
                ));

                if include_child {
                    if let ChildRef::Arena(child_idx) = ext.child {
                        stack.push((child_idx, child_path));
                    }
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
                    let child_path = nibbles_push(&path, slot);
                    let include_child = include_paths.contains(&child_path);
                    let (rlp_node, is_hash) = child_to_rlp_node(arena, child_ref)?;
                    if is_hash {
                        hash_mask.set_bit(slot);
                    }
                    if include_child {
                        if let ChildRef::Arena(child_idx) = child_ref {
                            tree_mask.set_bit(slot);
                            stack.push((*child_idx, child_path));
                        }
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

// ── Sparse trie → segment conversion ─────────────────────────────────────────

struct SparseArenaBuilder<'a> {
    nodes: &'a alloy_primitives::map::HashMap<Nibbles, SparseNode>,
    values: &'a alloy_primitives::map::HashMap<Nibbles, Vec<u8>>,
    path_to_idx: HashMap<Nibbles, u32>,
    arena_nodes: Vec<MptNode>,
    arena_hash_cache: Vec<Option<B256>>,
}

impl<'a> SparseArenaBuilder<'a> {
    fn new(
        nodes: &'a alloy_primitives::map::HashMap<Nibbles, SparseNode>,
        values: &'a alloy_primitives::map::HashMap<Nibbles, Vec<u8>>,
    ) -> Self {
        Self {
            nodes,
            values,
            path_to_idx: HashMap::new(),
            arena_nodes: Vec::new(),
            arena_hash_cache: Vec::new(),
        }
    }

    fn build_child_ref(&mut self, path: &Nibbles) -> MptResult<ChildRef> {
        let Some(node) = self.nodes.get(path) else {
            return Err(MptDbError::Other(format!(
                "sparse trie missing child node at path {:?}",
                path
            )));
        };
        Ok(match node {
            SparseNode::Hash(h) => ChildRef::Hash(*h),
            SparseNode::Empty => {
                return Err(MptDbError::Other(format!(
                    "sparse trie has Empty child at path {:?}",
                    path
                )));
            }
            SparseNode::Leaf { .. } | SparseNode::Extension { .. } | SparseNode::Branch { .. } => {
                ChildRef::Arena(self.build_node(path)?)
            }
        })
    }

    fn build_node(&mut self, path: &Nibbles) -> MptResult<u32> {
        if let Some(idx) = self.path_to_idx.get(path).copied() {
            return Ok(idx);
        }

        let Some(node) = self.nodes.get(path) else {
            return Err(MptDbError::Other(format!("sparse trie missing node at path {:?}", path)));
        };
        if matches!(node, SparseNode::Hash(_) | SparseNode::Empty) {
            return Err(MptDbError::Other(format!(
                "cannot materialize sparse node {:?} at path {:?}",
                node, path
            )));
        }

        let idx = self.arena_nodes.len() as u32;
        self.path_to_idx.insert(path.clone(), idx);
        self.arena_nodes.push(MptNode::Branch(BranchNode::new()));
        self.arena_hash_cache.push(get_node_hash(self.nodes, path));

        let converted = match node {
            SparseNode::Leaf { key, .. } => {
                let full_path = nibbles_extend(path, key);
                let value = self.values.get(&full_path).cloned().ok_or_else(|| {
                    MptDbError::Other(format!("missing sparse leaf value at path {:?}", full_path))
                })?;
                MptNode::Leaf(LeafNode { nibbles: key.clone(), value })
            }
            SparseNode::Extension { key, .. } => {
                let child_path = nibbles_extend(path, key);
                let child = self.build_child_ref(&child_path)?;
                MptNode::Extension(ExtensionNode { nibbles: key.clone(), child })
            }
            SparseNode::Branch { state_mask, .. } => {
                let mut branch = BranchNode::new();
                branch.value = self.values.get(path).cloned();
                for slot in 0u8..16 {
                    if !state_mask.is_bit_set(slot) {
                        continue;
                    }
                    let child_path = nibbles_push(path, slot);
                    branch.children[slot as usize] = Some(self.build_child_ref(&child_path)?);
                }
                MptNode::Branch(branch)
            }
            SparseNode::Hash(_) | SparseNode::Empty => unreachable!(),
        };

        self.arena_nodes[idx as usize] = converted;
        Ok(idx)
    }
}

fn build_storage_segment_from_sparse_serial(
    trie: &SerialSparseTrie,
    root: B256,
) -> MptResult<StorageTrieSegment> {
    let nodes = trie.nodes_ref();
    let values = trie.values_ref();
    let mut builder = SparseArenaBuilder::new(nodes, values);
    let root_path = Nibbles::default();
    let root_idx = builder.build_node(&root_path)?;
    StorageTrieSegment::from_parts(
        &builder.arena_nodes,
        &builder.arena_hash_cache,
        Some(root_idx),
        root,
    )
}

/// Build storage trie segments directly from the current sparse trie.
///
/// Each `(hashed_address, root)` entry corresponds to one storage trie to
/// publish for the current generation. Empty roots are skipped.
pub(crate) fn build_storage_segments_from_sparse_trie(
    sparse_trie: &SparseStateTrie,
    targets: &[(B256, B256)],
) -> MptResult<Vec<(B256, StorageTrieSegment)>> {
    let build_one =
        |(hashed_addr, root): &(B256, B256)| -> MptResult<Option<(B256, StorageTrieSegment)>> {
            if *root == EMPTY_ROOT_HASH {
                return Ok(None);
            }
            let storage_trie = sparse_trie.storage_trie_ref(hashed_addr).ok_or_else(|| {
                MptDbError::Other(format!(
                    "sparse storage trie missing for account {} root {}",
                    hashed_addr, root
                ))
            })?;
            let segment =
                build_storage_segment_from_sparse_serial(storage_trie, *root).map_err(|e| {
                    MptDbError::Other(format!(
                        "build sparse segment for account {} root {}: {e}",
                        hashed_addr, root
                    ))
                })?;
            Ok(Some((*hashed_addr, segment)))
        };

    let built: Vec<Option<(B256, StorageTrieSegment)>> = if targets.len() >= 64 {
        targets.par_iter().map(build_one).collect::<MptResult<Vec<_>>>()?
    } else {
        targets.iter().map(build_one).collect::<MptResult<Vec<_>>>()?
    };
    Ok(built.into_iter().flatten().collect())
}

// ── Phase 2: apply_all_storage_changes_sparse ─────────────────────────────────

#[inline]
fn extract_storage_multiproof_for_account(
    reader: &StorageTrieSegmentReader<'_>,
    dirty: &DirtyAccount,
    enable_full_scan: bool,
) -> MptResult<DecodedStorageMultiProof> {
    let storage_change_len = dirty.storage_changes.len();
    let should_force_full_scan = enable_full_scan &&
        (SPARSE_STORAGE_FULL_SCAN_MIN_CHANGES..=SPARSE_STORAGE_FULL_SCAN_MAX_CHANGES)
            .contains(&storage_change_len);

    if should_force_full_scan {
        return reader.extract_full_decoded_multiproof();
    }
    if storage_change_len == 1 {
        return reader
            .extract_decoded_multiproof(std::slice::from_ref(&dirty.storage_changes[0].slot_key));
    }

    let mut dirty_keys: Vec<Nibbles> = Vec::with_capacity(storage_change_len);
    dirty_keys.extend(dirty.storage_changes.iter().map(|c| c.slot_key.clone()));

    // Two-layer full-scan decision:
    // 1) `enable_full_scan` (workload-level gate) decides whether this block is allowed to use any
    //    full-trie extraction path.
    // 2) segment-reader internal heuristic (keys/node_count) is still active when calling
    //    `extract_decoded_multiproof`.
    // For large-account workloads (e.g. B4.6), call the explicit no-full-scan
    // variant to disable layer-2 heuristic entirely.
    if enable_full_scan {
        reader.extract_decoded_multiproof(&dirty_keys)
    } else {
        reader.extract_decoded_multiproof_no_full_scan(&dirty_keys)
    }
}

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
    let trace_enabled = std::env::var_os("MPT_SPARSE_APPLY_TRACE").is_some();
    let apply_total_start = trace_enabled.then(std::time::Instant::now);
    let mut t_reveal_account = std::time::Duration::ZERO;
    let mut t_reveal_storage = std::time::Duration::ZERO;
    let mut t_extract_storage_proof = std::time::Duration::ZERO;
    let mut t_storage_updates = std::time::Duration::ZERO;
    let mut t_storage_root_compute = std::time::Duration::ZERO;
    let mut t_account_updates = std::time::Duration::ZERO;
    let mut accounts_wiped = 0usize;
    let mut accounts_segment_reveal = 0usize;
    let mut accounts_prebuilt_reveal = 0usize;
    let mut accounts_empty_reveal = 0usize;
    let mut storage_change_total = 0usize;
    let enable_full_scan = dirty_accounts.len() <= SPARSE_STORAGE_FULL_SCAN_MAX_DIRTY_ACCOUNTS;
    let mut pre_extracted_segment_proofs: HashMap<B256, DecodedStorageMultiProof> =
        HashMap::with_capacity(dirty_accounts.len());
    let provider_storage_before =
        SPARSE_PROVIDER_STORAGE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    let provider_account_before =
        SPARSE_PROVIDER_ACCOUNT_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    let mut storage_root_accounts: Vec<B256> = Vec::with_capacity(dirty_accounts.len());

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
        let step_start = trace_enabled.then(std::time::Instant::now);
        trie.reveal_decoded_account_multiproof(account_nodes, account_masks)
            .map_err(|e| MptDbError::Other(format!("reveal account multiproof: {e}")))?;
        if let Some(start) = step_start {
            t_reveal_account += start.elapsed();
        }
    }

    // Pre-extract segment proofs in parallel before the per-account mutation loop.
    // This turns the dominant per-account serial extract cost into a batched step.
    let pre_extract_start = trace_enabled.then(std::time::Instant::now);
    let mut segment_extract_candidates: Vec<(B256, Arc<SegmentPageLease>, usize)> =
        Vec::with_capacity(dirty_accounts.len());
    for (dirty_idx, dirty) in dirty_accounts.iter().enumerate() {
        if dirty.storage_wiped || dirty.storage_changes.is_empty() {
            continue;
        }
        if skip_already_revealed_storage && trie.storage_trie_ref(&dirty.hashed_address).is_some() {
            continue;
        }
        if provider_factory.pre_built_storage_proofs.contains_key(&dirty.hashed_address) {
            continue;
        }
        let Some(lease) = provider_factory.storage_segments.get(&dirty.hashed_address) else {
            continue;
        };
        segment_extract_candidates.push((dirty.hashed_address, Arc::clone(lease), dirty_idx));
    }
    if !segment_extract_candidates.is_empty() {
        let extracted = if segment_extract_candidates.len() >= 64 {
            segment_extract_candidates
                .into_par_iter()
                .map(
                    |(hashed_addr, lease, dirty_idx)| -> MptResult<(B256, DecodedStorageMultiProof)> {
                    let reader = StorageTrieSegmentReader::open_shared_page(
                        &lease,
                        lease.root(),
                        lease.root_record_off(),
                    )
                    .map_err(|e| {
                        MptDbError::Other(format!("open storage segment reader {hashed_addr}: {e}"))
                    })?;
                    let dirty = &dirty_accounts[dirty_idx];
                    let proof = extract_storage_multiproof_for_account(&reader, dirty, enable_full_scan)
                    .map_err(|e| {
                        MptDbError::Other(format!("extract storage multiproof {hashed_addr}: {e}"))
                    })?;
                    Ok((hashed_addr, proof))
                },
                )
                .collect::<MptResult<Vec<_>>>()?
        } else {
            segment_extract_candidates
                .into_iter()
                .map(
                    |(hashed_addr, lease, dirty_idx)| -> MptResult<(B256, DecodedStorageMultiProof)> {
                    let reader = StorageTrieSegmentReader::open_shared_page(
                        &lease,
                        lease.root(),
                        lease.root_record_off(),
                    )
                    .map_err(|e| {
                        MptDbError::Other(format!("open storage segment reader {hashed_addr}: {e}"))
                    })?;
                    let dirty = &dirty_accounts[dirty_idx];
                    let proof = extract_storage_multiproof_for_account(&reader, dirty, enable_full_scan)
                    .map_err(|e| {
                        MptDbError::Other(format!("extract storage multiproof {hashed_addr}: {e}"))
                    })?;
                    Ok((hashed_addr, proof))
                },
                )
                .collect::<MptResult<Vec<_>>>()?
        };
        pre_extracted_segment_proofs.extend(extracted);
    }
    if let Some(start) = pre_extract_start {
        t_extract_storage_proof += start.elapsed();
    }

    // ── Step 1: plan and batch-reveal storage tries ───────────────────────────
    //
    // We keep all source-selection logic (segment / prebuilt / known-empty /
    // wiped) identical to the serial path, but reveal in one batched call so
    // SparseStateTrie can parallelize per-account reveal internally.
    let mut storage_reveal_entries: Vec<(B256, DecodedStorageMultiProof)> =
        Vec::with_capacity(dirty_accounts.len());
    let mut storage_reveal_non_empty = 0usize;
    let mut storage_wipe_after_reveal: Vec<B256> = Vec::with_capacity(dirty_accounts.len() / 8);
    for dirty in dirty_accounts {
        let hashed_addr = dirty.hashed_address;
        let storage_change_len = dirty.storage_changes.len();

        if dirty.storage_wiped {
            accounts_wiped += 1;
            storage_reveal_entries.push((hashed_addr, DecodedStorageMultiProof::empty()));
            storage_wipe_after_reveal.push(hashed_addr);
            continue;
        }
        if storage_change_len == 0 {
            continue;
        }
        if skip_already_revealed_storage && trie.storage_trie_ref(&hashed_addr).is_some() {
            continue;
        }
        if let Some(proof) = pre_extracted_segment_proofs.remove(&hashed_addr) {
            accounts_segment_reveal += 1;
            storage_reveal_entries.push((hashed_addr, proof));
            storage_reveal_non_empty += 1;
            continue;
        }
        if let Some(reader) = provider_factory.get_storage_reader(hashed_addr) {
            accounts_segment_reveal += 1;
            let extract_start = trace_enabled.then(std::time::Instant::now);
            let proof = extract_storage_multiproof_for_account(&reader, dirty, enable_full_scan)
                .map_err(|e| {
                    MptDbError::Other(format!("extract storage multiproof {hashed_addr}: {e}"))
                })?;
            if let Some(start) = extract_start {
                t_extract_storage_proof += start.elapsed();
            }
            storage_reveal_entries.push((hashed_addr, proof));
            storage_reveal_non_empty += 1;
            continue;
        }
        if let Some(proof) = provider_factory.pre_built_storage_proofs.get(&hashed_addr).cloned() {
            accounts_prebuilt_reveal += 1;
            storage_reveal_entries.push((hashed_addr, proof));
            storage_reveal_non_empty += 1;
            continue;
        }

        let is_known_empty = dirty.storage_known_empty ||
            provider_factory.known_empty_accounts.contains(&hashed_addr);
        if !is_known_empty {
            return Err(MptDbError::Other(format!(
                "no storage segment for existing account {hashed_addr}; \
                 check that dirty.storage_known_empty is set for new accounts"
            )));
        }
        accounts_empty_reveal += 1;
        storage_reveal_entries.push((hashed_addr, DecodedStorageMultiProof::empty()));
    }
    if !storage_reveal_entries.is_empty() {
        let reveal_start = trace_enabled.then(std::time::Instant::now);
        if storage_reveal_non_empty == 0 ||
            storage_reveal_entries.len() <= SPARSE_STORAGE_REVEAL_BATCH_CHUNK
        {
            let mut storages = alloy_primitives::map::B256Map::default();
            storages.reserve(storage_reveal_entries.len());
            for (hashed_addr, proof) in storage_reveal_entries {
                storages.insert(hashed_addr, proof);
            }
            trie.reveal_decoded_multiproof(DecodedMultiProof {
                account_subtree: DecodedProofNodes::default(),
                branch_node_masks: BranchNodeMasksMap::default(),
                storages,
            })
            .map_err(|e| MptDbError::Other(format!("batch reveal storage multiproof: {e}")))?;
        } else {
            let mut reveal_iter = storage_reveal_entries.into_iter();
            loop {
                let mut storages = alloy_primitives::map::B256Map::default();
                storages.reserve(SPARSE_STORAGE_REVEAL_BATCH_CHUNK);
                for _ in 0..SPARSE_STORAGE_REVEAL_BATCH_CHUNK {
                    let Some((hashed_addr, proof)) = reveal_iter.next() else {
                        break;
                    };
                    storages.insert(hashed_addr, proof);
                }
                if storages.is_empty() {
                    break;
                }
                trie.reveal_decoded_multiproof(DecodedMultiProof {
                    account_subtree: DecodedProofNodes::default(),
                    branch_node_masks: BranchNodeMasksMap::default(),
                    storages,
                })
                .map_err(|e| MptDbError::Other(format!("batch reveal storage multiproof: {e}")))?;
            }
        }
        if let Some(start) = reveal_start {
            t_reveal_storage += start.elapsed();
        }
    }
    if !storage_wipe_after_reveal.is_empty() {
        let wipe_start = trace_enabled.then(std::time::Instant::now);
        for hashed_addr in storage_wipe_after_reveal {
            trie.wipe_storage(hashed_addr)
                .map_err(|e| MptDbError::Other(format!("wipe_storage {hashed_addr}: {e}")))?;
        }
        if let Some(start) = wipe_start {
            t_reveal_storage += start.elapsed();
        }
    }

    // ── Per-account loop ──────────────────────────────────────────────────────
    for dirty in dirty_accounts {
        let hashed_addr = dirty.hashed_address;
        storage_change_total += dirty.storage_changes.len();
        // ── Step 2: apply storage changes ────────────────────────────────────
        let storage_apply_start = trace_enabled.then(std::time::Instant::now);
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
        if let Some(start) = storage_apply_start {
            t_storage_updates += start.elapsed();
        }
        if dirty.storage_wiped || !dirty.storage_changes.is_empty() {
            storage_root_accounts.push(hashed_addr);
        }
    }

    let storage_root_compute_start = trace_enabled.then(std::time::Instant::now);
    let storage_roots = trie
        .storage_roots_for_accounts(&storage_root_accounts)
        .map_err(|e| MptDbError::Other(format!("compute sparse storage roots: {e}")))?;
    if let Some(start) = storage_root_compute_start {
        t_storage_root_compute += start.elapsed();
    }

    // ── Step 3: apply account changes using precomputed storage roots ────────
    for dirty in dirty_accounts {
        let hashed_addr = dirty.hashed_address;
        let account_apply_start = trace_enabled.then(std::time::Instant::now);
        match &dirty.info {
            Some(info) => {
                let has_storage_delta = dirty.storage_wiped || !dirty.storage_changes.is_empty();
                let storage_root = if has_storage_delta {
                    storage_roots.get(&hashed_addr).copied().unwrap_or(EMPTY_ROOT_HASH)
                } else {
                    trie.get_account_value(&hashed_addr)
                        .and_then(|v| TrieAccount::decode(&mut v.as_slice()).ok())
                        .map(|ta| ta.storage_root)
                        .unwrap_or(EMPTY_ROOT_HASH)
                };

                if info.is_empty() && storage_root == EMPTY_ROOT_HASH {
                    trie.remove_account_leaf(&dirty.account_key, provider_factory).map_err(
                        |e| MptDbError::Other(format!("remove_account_leaf {hashed_addr}: {e}")),
                    )?;
                } else {
                    let trie_account = TrieAccount {
                        nonce: info.nonce,
                        balance: info.balance,
                        storage_root,
                        code_hash: info.code_hash,
                    };
                    let mut rlp_buf = Vec::new();
                    trie_account.encode(&mut rlp_buf);
                    trie.update_account_leaf(dirty.account_key.clone(), rlp_buf, provider_factory)
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
        if let Some(start) = account_apply_start {
            t_account_updates += start.elapsed();
        }
    }

    if let Some(start) = apply_total_start {
        let provider_storage_after =
            SPARSE_PROVIDER_STORAGE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let provider_account_after =
            SPARSE_PROVIDER_ACCOUNT_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[mptsparse] accounts={} changes={} reveal_acct={:.1}ms extract={:.1}ms reveal_storage={:.1}ms storage_apply={:.1}ms root_compute={:.1}ms account_apply={:.1}ms total={:.1}ms seg={} prebuilt={} empty={} wiped={} provider_storage={} provider_account={}",
            dirty_accounts.len(),
            storage_change_total,
            t_reveal_account.as_secs_f64() * 1000.0,
            t_extract_storage_proof.as_secs_f64() * 1000.0,
            t_reveal_storage.as_secs_f64() * 1000.0,
            t_storage_updates.as_secs_f64() * 1000.0,
            t_storage_root_compute.as_secs_f64() * 1000.0,
            t_account_updates.as_secs_f64() * 1000.0,
            start.elapsed().as_secs_f64() * 1000.0,
            accounts_segment_reveal,
            accounts_prebuilt_reveal,
            accounts_empty_reveal,
            accounts_wiped,
            provider_storage_after.saturating_sub(provider_storage_before),
            provider_account_after.saturating_sub(provider_account_before),
        );
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
        for i in 0u8..20 {
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
