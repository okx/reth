use alloy_primitives::B256;
use alloy_trie::Nibbles;
use mptdb_common::error::{MptDbError, Result};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use super::{
    arena::MutableTrieArena,
    encoding::decode_node,
    node::{BranchNode, ChildRef, ExtensionNode, LeafNode, MptNode},
    overlay::StorageOverlay,
    parallel::ParallelismThresholds,
    persisted::{self, PersistedTrieStore},
    segment::{
        SegmentChildEmbedRef, SegmentNodeKind, SegmentNodeRef, SegmentPageLease,
        StorageTrieSegment, StorageTrieSegmentReader,
    },
    state::StorageChange,
    storage_recompute,
    tree::MptTree,
    tree_algo,
};

static COW_DIAG_ENSURE_PATH_CALLS: AtomicU64 = AtomicU64::new(0);
static COW_DIAG_MUTATE_SEGMENT_NODE_CALLS: AtomicU64 = AtomicU64::new(0);
static COW_DIAG_APPLY_CALLS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn cow_diag_enabled() -> bool {
    std::env::var_os("MPT_DEBUG_STORAGE_COW_DIAG").is_some()
}

#[derive(Clone)]
pub enum CowLazyNodeRef {
    Persisted(B256),
    Inline(Vec<u8>),
    Segment(SegmentNodeRef),
}

#[derive(Clone)]
pub enum CowRootRef {
    Empty,
    Arena(u32),
    Lazy(CowLazyNodeRef),
}

#[derive(Clone)]
pub enum CowChildRef {
    Arena(u32),
    Lazy(CowLazyNodeRef),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PendingCowEdge {
    Extension,
    Branch(u8),
}

#[derive(Clone)]
pub struct StorageTrieCow {
    root: CowRootRef,
    arena: MutableTrieArena,
    pending_lazy_children: HashMap<(u32, PendingCowEdge), CowChildRef>,
}

impl StorageTrieCow {
    const ROOT_HASH_ONLY_PARALLEL_MIN_ARENA_NODES: usize = 4096;

    pub fn empty() -> Self {
        Self {
            root: CowRootRef::Empty,
            arena: MutableTrieArena::new(),
            pending_lazy_children: HashMap::new(),
        }
    }

    /// Clone only the frozen base, with zero-capacity overlay structures.
    ///
    /// Cheaper than a full `clone()` when the caller intends to immediately
    /// call `steal_overlay_capacity_from` to transfer pre-allocated backing
    /// from the donor handle.
    pub fn clone_frozen_only(&self) -> Self {
        Self {
            root: self.root.clone(),
            arena: self.arena.clone_frozen_only(),
            pending_lazy_children: HashMap::new(),
        }
    }

    /// Transfer pre-allocated overlay capacity from `donor` into self.
    ///
    /// Transfers arena overlay backing AND `pending_lazy_children` from donor
    /// into self.  Donor's overlays must be empty (drained by `snapshot()` +
    /// `clear_pending_lazy()` via `set_committed_base`).  After the transfer,
    /// donor holds zero-capacity structures (O(1) drop) while self has
    /// pre-allocated backing (no HashMap resizes on first block of writes).
    /// Returns true if the overlay is in a reusable state for steal.
    pub fn is_overlay_reusable(&self) -> bool {
        // pending_lazy_children must be empty before steal.  Values can contain
        // CowChildRef::Lazy(CowLazyNodeRef::Inline(Vec<u8>)) — heap allocations
        // from the calling thread.  Swapping a non-empty map across rayon
        // thread boundaries causes cross-thread deallocation which triggers
        // jemalloc guard pages.  set_committed_base -> clear_pending_lazy()
        // ensures this is empty in the normal commit flow.
        self.arena.is_overlay_reusable() && self.pending_lazy_children.is_empty()
    }

    /// Transfer overlay capacity from donor.  If `watermark_target` is Some,
    /// shrink the transferred capacity when it greatly exceeds the expected
    /// usage for the next block (watermark policy: shrink if capacity > 4×target).
    /// Returns true if a shrink was triggered.
    pub fn steal_overlay_capacity_from(
        &mut self,
        donor: &mut Self,
        watermark_target: Option<usize>,
    ) -> bool {
        self.arena.steal_overlay_capacity_from(&mut donor.arena);
        let shrank = if let Some(target) = watermark_target {
            self.arena.shrink_overlay_if_oversized(target)
        } else {
            false
        };
        // Transfer pending_lazy_children capacity from donor.
        // Safe to swap only when donor is empty: values can contain
        // CowChildRef::Lazy(CowLazyNodeRef::Inline(Vec<u8>)) — cross-thread
        // deallocation of non-empty maps triggers jemalloc guard pages.
        // is_overlay_reusable() already guarantees empty when steal is reached,
        // but double-check here for defence in depth.
        debug_assert!(
            donor.pending_lazy_children.is_empty(),
            "pending_lazy_children must be empty before steal (clear_pending_lazy not called?)"
        );
        if donor.pending_lazy_children.is_empty() {
            std::mem::swap(&mut self.pending_lazy_children, &mut donor.pending_lazy_children);
        }
        shrank
    }

    pub fn overlay_capacity(&self) -> usize {
        self.arena.overlay_capacity()
    }

    pub fn from_segment_page(page: Arc<SegmentPageLease>) -> Self {
        let reader =
            StorageTrieSegmentReader::open_shared_page(&page, page.root(), page.root_record_off())
                .expect("segment page lease should always reference a valid trie page");
        let root = reader.root_ref().expect("segment page lease should expose a root node ref");
        let node_count = reader.node_count();
        // Pre-allocate overlay capacity based on the segment's node count.
        // L3-loaded tries start from scratch (no steal_overlay_capacity_from
        // benefit). Without pre-sizing, preload_batched_paths triggers 7-8
        // HashMap resizes per ~50-node trie. With pre-sizing, no resizes occur.
        let mut arena = MutableTrieArena::new();
        if node_count > 0 {
            arena.reserve_overlay_entries(node_count);
        }
        Self {
            root: CowRootRef::Lazy(CowLazyNodeRef::Segment(root)),
            arena,
            pending_lazy_children: HashMap::new(),
        }
    }

    pub fn from_segment_root(root: SegmentNodeRef) -> Self {
        Self {
            root: CowRootRef::Lazy(CowLazyNodeRef::Segment(root)),
            arena: MutableTrieArena::new(),
            pending_lazy_children: HashMap::new(),
        }
    }

    pub fn from_persisted_root(root: B256) -> Self {
        Self {
            root: CowRootRef::Lazy(CowLazyNodeRef::Persisted(root)),
            arena: MutableTrieArena::new(),
            pending_lazy_children: HashMap::new(),
        }
    }

    pub fn from_tree(tree: MptTree) -> Self {
        let root = tree.root.map(CowRootRef::Arena).unwrap_or(CowRootRef::Empty);
        Self { root, arena: tree.arena, pending_lazy_children: HashMap::new() }
    }

    pub fn into_snapshot_cached(
        mut self,
        storage_root: B256,
        published_segment: Option<&StorageTrieSegment>,
        use_async: bool,
    ) -> Result<Self> {
        if storage_root == alloy_trie::EMPTY_ROOT_HASH {
            return Ok(Self::empty());
        }

        // If a pre-built published segment is available (sync path), convert
        // to lazy segment reference immediately — keeps L2 cache lightweight.
        if let Some(segment) = published_segment {
            return Ok(Self::from_segment_page(segment.clone().into_page_lease()));
        }

        // Async/wal_first hot path: keep the trie as-is in L2 cache and avoid
        // front-end segment serialization/materialization work. Segment build
        // is handled by the background publish worker.
        if use_async {
            self.clear_dirty();
            return Ok(self);
        }

        // Sync inline-segment path: before serializing, materialize pending
        // segment-lazy children into arena refs so segment encoding does not
        // downgrade them into hash embeds that require persisted fallback.
        self.materialize_pending_segment_children()?;

        // Sync path only: build segment inline so L2 stores a lightweight
        // lazy reference instead of the full materialized arena.
        if let Some(root_idx) = self.root_index() {
            let nodes = self.arena_nodes();
            let hashes = self.arena_hash_cache();
            let segment =
                StorageTrieSegment::from_parts(&nodes, &hashes, Some(root_idx), storage_root)?;
            return Ok(Self::from_segment_page(segment.into_page_lease()));
        }

        self.clear_dirty();
        Ok(self)
    }

    pub fn root_ref(&self) -> &CowRootRef {
        &self.root
    }

    pub fn arena(&self) -> &MutableTrieArena {
        &self.arena
    }

    pub fn clear_dirty(&mut self) {
        self.arena.clear_all_dirty();
    }

    /// Snapshot the arena so that future `clone()` is O(overlay_size).
    /// Call after commit when this trie becomes the new baseline.
    /// Unlike freeze() (which is O(total_nodes)), snapshot() only promotes
    /// the overlay without copying unchanged base nodes.
    pub fn snapshot(&mut self) {
        self.arena.snapshot();
    }

    /// Clear pending lazy children.  After commit, all accessed paths are
    /// materialized in the arena — stale lazy refs are no longer needed.
    pub fn clear_pending_lazy(&mut self) {
        self.pending_lazy_children.clear();
    }

    /// Materialize pending segment-lazy children into arena refs.
    ///
    /// This is required before segment serialization (`from_parts`) so edges
    /// tracked only in `pending_lazy_children` are not silently emitted as
    /// hash embeds.
    pub fn materialize_pending_segment_children(&mut self) -> Result<()> {
        if self.pending_lazy_children.is_empty() {
            return Ok(());
        }

        let pending = std::mem::take(&mut self.pending_lazy_children);
        for ((parent_idx, edge), child_ref) in pending {
            let CowChildRef::Lazy(CowLazyNodeRef::Segment(node_ref)) = child_ref else {
                self.pending_lazy_children.insert((parent_idx, edge), child_ref);
                continue;
            };

            if (parent_idx as usize) >= self.arena.len() {
                continue;
            }

            let child_idx = self.materialize_segment_lazy_subtree(node_ref)?;
            match (self.arena.get_mut(parent_idx), edge) {
                (MptNode::Extension(ext), PendingCowEdge::Extension) => {
                    ext.child = ChildRef::Arena(child_idx);
                }
                (MptNode::Branch(branch), PendingCowEdge::Branch(slot)) => {
                    branch.children[slot as usize] = Some(ChildRef::Arena(child_idx));
                }
                _ => {}
            }
        }

        self.prune_pending_lazy_children();
        Ok(())
    }

    /// Collect all arena nodes into a contiguous Vec.
    ///
    /// Only used by low-frequency paths (segment build, snapshot export).
    pub fn arena_nodes(&self) -> Vec<MptNode> {
        self.arena.collect_all_nodes()
    }

    /// Collect the hash cache, merging frozen base + overlay.
    pub fn arena_hash_cache(&self) -> Vec<Option<B256>> {
        let len = self.arena.len();
        (0..len).map(|i| self.arena.get_hash(i as u32)).collect()
    }

    /// Reference to the frozen base nodes — zero-copy.
    ///
    /// **Must be called after `snapshot()`** so that all nodes are consolidated
    /// into the frozen base.  Used by background workers to avoid the extra
    /// allocation of `arena_nodes()`.
    pub fn frozen_arena_nodes_ref(&self) -> &[MptNode] {
        self.arena.frozen_nodes_ref()
    }

    /// Reference to the frozen base hash cache — zero-copy.
    ///
    /// **Must be called after `snapshot()`**.
    pub fn frozen_arena_hash_cache_ref(&self) -> &[Option<B256>] {
        self.arena.frozen_hash_cache_ref()
    }

    /// Number of arena nodes.
    pub fn arena_len(&self) -> usize {
        self.arena.len()
    }

    /// Returns true if the root is a lazy reference (segment or persisted hash)
    /// with an empty arena — i.e., the trie has never been materialized.
    pub fn is_lazy_root(&self) -> bool {
        matches!(self.root, CowRootRef::Lazy(_)) && self.arena.is_empty()
    }

    pub fn root_index(&self) -> Option<u32> {
        match self.root {
            CowRootRef::Empty => None,
            CowRootRef::Arena(idx) => Some(idx),
            CowRootRef::Lazy(_) => None,
        }
    }

    /// Collect all leaf entries as `(full_nibble_key, value)` pairs.
    ///
    /// Only works when the trie is fully materialized in-memory (Arena root).
    /// Returns an empty vec for lazy or empty tries.
    pub fn collect_leaf_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some(root_idx) = self.root_index() else {
            return Vec::new();
        };
        let tree = super::tree::MptTree { arena: self.arena.clone(), root: Some(root_idx) };
        tree.collect_leaf_entries()
    }

    pub fn get(&self, store: &PersistedTrieStore, key: &Nibbles) -> Result<Option<Vec<u8>>> {
        match self.root_ref() {
            CowRootRef::Empty => Ok(None),
            CowRootRef::Arena(idx) => Ok(self.get_arena_recursive(*idx, store, key, 0)),
            CowRootRef::Lazy(lazy) => self.get_lazy_recursive(lazy.clone(), store, key, 0),
        }
    }

    pub fn apply_change(
        &mut self,
        store: &PersistedTrieStore,
        key: &Nibbles,
        value: Option<Vec<u8>>,
    ) -> Result<()> {
        let new_root = match value {
            Some(value) => {
                let root_idx = self.ensure_path_loaded(store, key)?;
                let current_root = root_idx.or_else(|| match self.root {
                    CowRootRef::Arena(idx) => Some(idx),
                    CowRootRef::Empty => None,
                    _ => None,
                });
                Some(tree_algo::insert_recursive(&mut self.arena, current_root, key, 0, value))
            }
            None => {
                let root_idx = self.ensure_delete_ready(store, key)?;
                tree_algo::delete_recursive(&mut self.arena, root_idx, key, 0).1
            }
        };
        self.root = match new_root {
            Some(idx) => CowRootRef::Arena(idx),
            None => CowRootRef::Empty,
        };
        self.prune_pending_lazy_children();
        Ok(())
    }

    /// Fast path for fully materialized tries (all children are Arena refs).
    ///
    /// Skips `ensure_path_loaded` / `ensure_delete_ready` / `prune_pending_lazy_children`
    /// which are no-ops for materialized tries.  Eliminates the redundant
    /// pre-walk (each level clones MptNode just to check child type), cutting
    /// per-update cost roughly in half.
    pub fn apply_change_materialized(&mut self, key: &Nibbles, value: Option<Vec<u8>>) {
        let root_idx = match self.root {
            CowRootRef::Arena(idx) => Some(idx),
            CowRootRef::Empty => None,
            CowRootRef::Lazy(_) => panic!("apply_change_materialized: root is Lazy"),
        };
        let new_root = match value {
            Some(value) => {
                Some(tree_algo::insert_recursive(&mut self.arena, root_idx, key, 0, value))
            }
            None => tree_algo::delete_recursive(&mut self.arena, root_idx, key, 0).1,
        };
        self.root = match new_root {
            Some(idx) => CowRootRef::Arena(idx),
            None => CowRootRef::Empty,
        };
    }

    pub fn apply_changes_batched(
        &mut self,
        store: &PersistedTrieStore,
        changes: &[StorageChange],
    ) -> Result<()> {
        self.apply_changes_batched_inner(store, changes)
    }

    fn apply_changes_batched_inner(
        &mut self,
        store: &PersistedTrieStore,
        changes: &[StorageChange],
    ) -> Result<()> {
        let diag = cow_diag_enabled();
        let diag_start = diag.then(std::time::Instant::now);
        let ensure_before = diag.then(|| COW_DIAG_ENSURE_PATH_CALLS.load(Ordering::Relaxed));
        let seg_before = diag.then(|| COW_DIAG_MUTATE_SEGMENT_NODE_CALLS.load(Ordering::Relaxed));
        let pending_before = self.pending_lazy_children.len();

        if changes.is_empty() {
            return Ok(());
        }

        // Fast path for fully materialized tries (Arena root, all children
        // are Arena refs).  Skips sort/dedup/preload_batched_paths (only
        // useful for lazy roots) and uses apply_change_materialized which
        // eliminates the redundant ensure_path_loaded pre-walk.
        if !self.is_lazy_root() && self.pending_lazy_children.is_empty() {
            for change in changes {
                let value = if change.value == alloy_primitives::U256::ZERO {
                    None
                } else {
                    change.encoded_value.clone()
                };
                self.apply_change_materialized(&change.slot_key, value);
            }
            return Ok(());
        }

        let mut ordered: Vec<&StorageChange> = changes.iter().collect();
        ordered.sort_by(|a, b| a.slot_key.cmp(&b.slot_key));

        // Collect touched keys and compute has_deletes on the EFFECTIVE
        // (deduplicated) changes only.  When the same slot appears multiple
        // times, the last entry wins; a slot that ends non-zero is not a delete
        // even if an intermediate entry was zero.
        let mut touched_keys = Vec::with_capacity(ordered.len());
        let mut has_deletes = false;
        let mut idx = 0usize;
        while idx < ordered.len() {
            let change = ordered[idx];
            // Skip to the last entry for this slot key (last write wins).
            while idx + 1 < ordered.len() && ordered[idx + 1].slot_key == change.slot_key {
                idx += 1;
            }
            let effective = ordered[idx];
            touched_keys.push(effective.slot_key.clone());
            if effective.value == alloy_primitives::U256::ZERO {
                has_deletes = true;
            }
            idx += 1;
        }

        // Deletes can trigger branch collapse which needs untouched siblings.
        // trace_paths only materializes touched paths, leaving siblings as
        // ChildRef::Hash — collapse can't access them.  Skip batch preload
        // when effective deletes exist; apply_change materializes on-demand.
        self.preload_batched_paths(store, &touched_keys, has_deletes)?;

        let mut idx = 0usize;
        while idx < ordered.len() {
            let change = ordered[idx];
            while idx + 1 < ordered.len() && ordered[idx + 1].slot_key == change.slot_key {
                idx += 1;
            }
            let change = ordered[idx];
            let value = if change.value == alloy_primitives::U256::ZERO {
                None
            } else {
                change.encoded_value.clone()
            };
            self.apply_change(store, &change.slot_key, value)?;
            idx += 1;
        }

        if let (true, Some(start), Some(ensure0), Some(seg0)) =
            (diag, diag_start, ensure_before, seg_before)
        {
            let apply_call = COW_DIAG_APPLY_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
            let should_log = apply_call <= 20 || apply_call % 2000 == 0;
            if !should_log {
                return Ok(());
            }
            let ensure_delta = COW_DIAG_ENSURE_PATH_CALLS.load(Ordering::Relaxed) - ensure0;
            let seg_delta = COW_DIAG_MUTATE_SEGMENT_NODE_CALLS.load(Ordering::Relaxed) - seg0;
            eprintln!(
                "[cowdiag] apply#{} changes={} touched={} deletes={} pending:{}->{} ensure_calls={} seg_materialize={} elapsed={:?}",
                apply_call,
                changes.len(),
                touched_keys.len(),
                has_deletes,
                pending_before,
                self.pending_lazy_children.len(),
                ensure_delta,
                seg_delta,
                start.elapsed()
            );
        }

        Ok(())
    }

    pub fn preload_paths(
        &mut self,
        store: &PersistedTrieStore,
        touched_keys: &[Nibbles],
    ) -> Result<()> {
        self.preload_batched_paths(store, touched_keys, false)
    }

    fn preload_batched_paths(
        &mut self,
        store: &PersistedTrieStore,
        touched_keys: &[Nibbles],
        has_deletes: bool,
    ) -> Result<()> {
        if touched_keys.is_empty() {
            return Ok(());
        }

        match self.root.clone() {
            CowRootRef::Lazy(CowLazyNodeRef::Segment(root_ref))
                if self.arena.is_empty() && self.pending_lazy_children.is_empty() =>
            {
                if has_deletes {
                    // Deletes can trigger branch collapse which needs untouched
                    // siblings. trace_paths() only materializes touched paths
                    // and leaves siblings as ChildRef::Hash — collapse can't
                    // access them. Skip batch preload; apply_change() will
                    // materialize on-demand via pending_lazy_children.
                    let _ = root_ref;
                } else {
                    // Pure updates (no deletes): batch-preload touched paths from
                    // the segment. This is much faster than per-slot on-demand
                    // materialization (~3x for B4.6).
                    //
                    // After trace_paths, untouched siblings remain as
                    // ChildRef::Hash in the arena. We populate pending_lazy_children
                    // with their SegmentNodeRef so that any structural access
                    // (e.g. branch collapse) loads from segment mmap instead of
                    // the persisted store. This is required for wal_first mode
                    // where the persisted store has not been updated yet.
                    let reader = StorageTrieSegmentReader::open_shared_page(
                        root_ref.page_lease(),
                        root_ref.page_lease().root(),
                        root_ref.page_lease().root_record_off(),
                    )?;
                    let trace = reader.cursor().trace_paths(touched_keys)?;
                    let (arena, root, lazy_siblings) = trace.into_parts();
                    self.arena = arena;
                    self.root = root.map(CowRootRef::Arena).unwrap_or(CowRootRef::Empty);

                    // Filter: only add entries for children that are still
                    // ChildRef::Hash. Touched children are already ChildRef::Arena
                    // and must not be shadowed by a stale lazy entry.
                    for (parent_arena_idx, slot_opt, node_ref) in lazy_siblings {
                        let edge = match slot_opt {
                            None => PendingCowEdge::Extension,
                            Some(s) => PendingCowEdge::Branch(s),
                        };
                        let is_hash = match slot_opt {
                            None => matches!(
                                self.arena.get(parent_arena_idx),
                                MptNode::Extension(ext) if matches!(ext.child, ChildRef::Hash(_))
                            ),
                            Some(s) => matches!(
                                self.arena.get(parent_arena_idx),
                                MptNode::Branch(branch) if matches!(
                                    branch.children[s as usize],
                                    Some(ChildRef::Hash(_))
                                )
                            ),
                        };
                        if is_hash {
                            self.pending_lazy_children.insert(
                                (parent_arena_idx, edge),
                                CowChildRef::Lazy(CowLazyNodeRef::Segment(node_ref)),
                            );
                        }
                    }
                }
            }
            CowRootRef::Lazy(CowLazyNodeRef::Persisted(root))
                if self.arena.is_empty() && self.pending_lazy_children.is_empty() =>
            {
                let tree = persisted::load_tree_paths_from_root(store, root, touched_keys)?;
                *self = Self::from_tree(tree);
            }
            // Arena root: no bulk preload needed.  Reads resolve lazy
            // children on-demand (mmap segment or persisted store) and
            // writes COW only the modified path via ensure_path_loaded.
            _ => {}
        }

        Ok(())
    }

    /// Compute the storage root hash, returning the updated trie.
    ///
    /// Materialises the trie into a fully arena-backed `MptTree`, computes the
    /// root hash via `MptTree::root_hash`, and converts back to `StorageTrieCow`.
    /// Compute root hash from the arena WITHOUT materialising lazy/segment/persisted nodes.
    ///
    /// For arena-rooted tries: hashes are computed recursively from the arena.
    ///   - `ChildRef::Hash(h)` children contribute `h` directly — no store access.
    ///   - This preserves `pending_lazy_children` so subsequent `apply_change` calls can still
    ///     resolve segment-backed hash nodes.
    /// For lazy roots (segment or persisted): the cached root hash is returned directly.
    pub fn root_hash_only(mut self, _store: &PersistedTrieStore) -> Result<(B256, StorageTrieCow)> {
        let root = match self.root {
            CowRootRef::Empty => alloy_trie::EMPTY_ROOT_HASH,
            CowRootRef::Arena(idx) => {
                // Build an MptTree from the owned arena and compute hash in-place.
                // Avoid cloning the whole arena on the hot commit path.
                let arena = std::mem::replace(&mut self.arena, MutableTrieArena::new());
                let mut tree = MptTree { arena, root: Some(idx) };
                let h = tree.root_hash();
                // Propagate computed hashes back so the next block's proof
                // extraction can read arena.get_hash(idx).
                self.arena = tree.arena;
                h
            }
            CowRootRef::Lazy(CowLazyNodeRef::Persisted(h)) => h,
            CowRootRef::Lazy(CowLazyNodeRef::Segment(ref node_ref)) => {
                node_ref.hash().unwrap_or(alloy_trie::EMPTY_ROOT_HASH)
            }
            CowRootRef::Lazy(CowLazyNodeRef::Inline(ref rlp)) => super::hash::hash_rlp(rlp),
        };
        self.arena.snapshot();
        Ok((root, self))
    }

    /// Parallel variant — delegates to serial (no parallel optimisation needed).
    pub fn root_hash_only_parallel(
        mut self,
        store: &PersistedTrieStore,
    ) -> Result<(B256, StorageTrieCow)> {
        if let CowRootRef::Arena(idx) = self.root {
            let arena = std::mem::replace(&mut self.arena, MutableTrieArena::new());
            let mut tree = MptTree { arena, root: Some(idx) };
            let root = if tree.arena_len() >= Self::ROOT_HASH_ONLY_PARALLEL_MIN_ARENA_NODES {
                tree.root_hash_parallel_hash_cache_only(&ParallelismThresholds::default())
            } else {
                tree.root_hash()
            };
            self.arena = tree.arena;
            self.arena.snapshot();
            return Ok((root, self));
        }
        self.root_hash_only(store)
    }

    /// Account-trie hash-only parallel path (legacy recompute kernel).
    ///
    /// This intentionally mirrors the pre-sparse account-root computation path:
    /// materialize lazy root if needed, then run `storage_recompute::recompute_hash_only_parallel`
    /// and update hash cache in-place. No dirty blob collection.
    pub fn root_hash_only_parallel_account(
        mut self,
        store: &PersistedTrieStore,
    ) -> Result<(B256, StorageTrieCow)> {
        let root = match self.root.clone() {
            CowRootRef::Empty => None,
            CowRootRef::Arena(idx) => Some(idx),
            CowRootRef::Lazy(_) => self.materialize_root_subtree(store, self.root.clone())?,
        };
        self.root = match root {
            Some(idx) => CowRootRef::Arena(idx),
            None => CowRootRef::Empty,
        };
        self.prune_pending_lazy_children();
        let hash = storage_recompute::recompute_hash_only_parallel(&mut self.arena, root);
        Ok((hash, self))
    }

    /// Compute root hash and collect dirty node blobs for RocksDB persistence.
    pub fn root_hash_and_dirty_blobs(
        self,
        store: &PersistedTrieStore,
    ) -> Result<(B256, Vec<(alloy_primitives::B256, Vec<u8>)>, StorageTrieCow)> {
        let mut tree = self.into_materialized_tree(store)?;
        let (root, blobs) = tree.root_hash_and_dirty_blobs();
        Ok((root, blobs, StorageTrieCow::from_tree(tree)))
    }

    /// Parallel variant — delegates to serial.
    pub fn root_hash_and_dirty_blobs_parallel(
        self,
        store: &PersistedTrieStore,
    ) -> Result<(B256, Vec<(alloy_primitives::B256, Vec<u8>)>, StorageTrieCow)> {
        self.root_hash_and_dirty_blobs(store)
    }

    pub fn into_overlay_materialized(
        mut self,
        store: &PersistedTrieStore,
    ) -> Result<StorageOverlay> {
        let root = self.materialize_root_subtree(store, self.root.clone())?;
        self.root = match root {
            Some(idx) => CowRootRef::Arena(idx),
            None => CowRootRef::Empty,
        };
        self.prune_pending_lazy_children();
        Ok(StorageOverlay::from_tree(MptTree { arena: self.arena, root }))
    }

    pub fn into_materialized_tree(mut self, store: &PersistedTrieStore) -> Result<MptTree> {
        let root = self.materialize_root_subtree(store, self.root.clone())?;
        self.root = match root {
            Some(idx) => CowRootRef::Arena(idx),
            None => CowRootRef::Empty,
        };
        self.prune_pending_lazy_children();
        Ok(MptTree {
            arena: self.arena,
            root: match self.root {
                CowRootRef::Arena(idx) => Some(idx),
                CowRootRef::Empty => None,
                CowRootRef::Lazy(_) => unreachable!("materialized tree cannot keep lazy root"),
            },
        })
    }

    pub fn clone_materialized_tree_if_ready(&self) -> Option<MptTree> {
        if !self.pending_lazy_children.is_empty() {
            return None;
        }
        let root = match self.root {
            CowRootRef::Arena(idx) => Some(idx),
            CowRootRef::Empty => None,
            CowRootRef::Lazy(_) => return None,
        };
        Some(MptTree { arena: self.arena.clone(), root })
    }

    pub fn reserve_for_expected_updates(&mut self, expected_changes: usize) {
        if expected_changes == 0 {
            return;
        }
        let reserve = expected_changes.saturating_mul(4);
        self.arena.reserve_overlay_entries(reserve);
    }

    fn get_arena_recursive(
        &self,
        idx: u32,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Option<Vec<u8>> {
        match self.arena.get(idx) {
            MptNode::Leaf(leaf) => {
                let remaining = key.slice(offset..);
                if leaf.nibbles == remaining {
                    Some(leaf.value.clone())
                } else {
                    None
                }
            }
            MptNode::Extension(ext) => {
                let remaining = key.slice(offset..);
                if remaining.len() < ext.nibbles.len() ||
                    remaining.slice(..ext.nibbles.len()) != ext.nibbles
                {
                    return None;
                }
                self.get_child_value(
                    idx,
                    PendingCowEdge::Extension,
                    &ext.child,
                    store,
                    key,
                    offset + ext.nibbles.len(),
                )
            }
            MptNode::Branch(branch) => {
                if offset >= key.len() {
                    branch.value.clone()
                } else {
                    let nibble = key.get_unchecked(offset) as usize;
                    let child = branch.children[nibble].as_ref()?;
                    self.get_child_value(
                        idx,
                        PendingCowEdge::Branch(nibble as u8),
                        child,
                        store,
                        key,
                        offset + 1,
                    )
                }
            }
        }
    }

    fn get_child_value(
        &self,
        parent_idx: u32,
        edge: PendingCowEdge,
        child: &ChildRef,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Option<Vec<u8>> {
        match self.pending_lazy_children.get(&(parent_idx, edge)) {
            Some(CowChildRef::Arena(idx)) => self.get_arena_recursive(*idx, store, key, offset),
            Some(CowChildRef::Lazy(lazy)) => {
                self.get_lazy_recursive(lazy.clone(), store, key, offset).ok().flatten()
            }
            None => match child {
                ChildRef::Arena(idx) => self.get_arena_recursive(*idx, store, key, offset),
                other => self
                    .get_lazy_recursive(child_ref_to_lazy(other.clone()), store, key, offset)
                    .ok()
                    .flatten(),
            },
        }
    }

    fn get_lazy_recursive(
        &self,
        lazy: CowLazyNodeRef,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Result<Option<Vec<u8>>> {
        match lazy {
            CowLazyNodeRef::Persisted(root) => {
                self.get_persisted_recursive(root, store, key, offset)
            }
            CowLazyNodeRef::Inline(rlp) => self.get_inline_recursive(&rlp, store, key, offset),
            CowLazyNodeRef::Segment(root) => self.get_segment_recursive(root, store, key, offset),
        }
    }

    fn get_segment_recursive(
        &self,
        node_ref: SegmentNodeRef,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Result<Option<Vec<u8>>> {
        let reader = StorageTrieSegmentReader::open_shared_page(
            node_ref.page_lease(),
            node_ref.page_lease().root(),
            node_ref.page_lease().root_record_off(),
        )?;
        let view = reader.view_node(node_ref.seg_idx())?;
        match view.kind {
            SegmentNodeKind::Leaf { nibbles, value } => {
                let remaining = key.slice(offset..);
                if remaining == Nibbles::from_nibbles(nibbles) {
                    Ok(Some(value.to_vec()))
                } else {
                    Ok(None)
                }
            }
            SegmentNodeKind::Extension { nibbles, child } => {
                let remaining = key.slice(offset..);
                if remaining.len() < nibbles.len() ||
                    remaining.slice(..nibbles.len()) != Nibbles::from_nibbles(nibbles)
                {
                    return Ok(None);
                }
                let next_offset = offset + nibbles.len();
                match child.target_idx {
                    Some(seg_idx) => {
                        let hash = reader.view_node(seg_idx)?.hash;
                        self.get_segment_recursive(
                            SegmentNodeRef::new(Arc::clone(node_ref.page_lease()), seg_idx, hash),
                            store,
                            key,
                            next_offset,
                        )
                    }
                    None => self.get_segment_embed(child.embed, store, key, next_offset),
                }
            }
            SegmentNodeKind::Branch { value, children, .. } => {
                if offset >= key.len() {
                    Ok(value.map(|value| value.to_vec()))
                } else {
                    let nibble = key.get_unchecked(offset);
                    for child in children.iter() {
                        let child = child?;
                        if child.slot == nibble {
                            return match child.target_idx {
                                Some(seg_idx) => {
                                    let hash = reader.view_node(seg_idx)?.hash;
                                    self.get_segment_recursive(
                                        SegmentNodeRef::new(
                                            Arc::clone(node_ref.page_lease()),
                                            seg_idx,
                                            hash,
                                        ),
                                        store,
                                        key,
                                        offset + 1,
                                    )
                                }
                                None => self.get_segment_embed(child.embed, store, key, offset + 1),
                            };
                        }
                    }
                    Ok(None)
                }
            }
        }
    }

    fn get_segment_embed(
        &self,
        embed: SegmentChildEmbedRef<'_>,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Result<Option<Vec<u8>>> {
        match embed {
            SegmentChildEmbedRef::None => Ok(None),
            SegmentChildEmbedRef::Hash(hash) => {
                self.get_persisted_recursive(hash, store, key, offset)
            }
            SegmentChildEmbedRef::Inline(bytes) => {
                self.get_inline_recursive(bytes, store, key, offset)
            }
        }
    }

    fn get_persisted_recursive(
        &self,
        root: B256,
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Result<Option<Vec<u8>>> {
        let rlp = match store.get_node(root)? {
            Some(rlp) => rlp,
            None => return Ok(None),
        };
        self.get_inline_recursive(&rlp, store, key, offset)
    }

    fn get_inline_recursive(
        &self,
        rlp: &[u8],
        store: &PersistedTrieStore,
        key: &Nibbles,
        offset: usize,
    ) -> Result<Option<Vec<u8>>> {
        let node =
            decode_node(rlp).map_err(|e| MptDbError::Other(format!("decode inline child: {e}")))?;
        match node {
            MptNode::Leaf(leaf) => {
                let remaining = key.slice(offset..);
                if leaf.nibbles == remaining {
                    Ok(Some(leaf.value))
                } else {
                    Ok(None)
                }
            }
            MptNode::Extension(ext) => {
                let remaining = key.slice(offset..);
                if remaining.len() < ext.nibbles.len() ||
                    remaining.slice(..ext.nibbles.len()) != ext.nibbles
                {
                    return Ok(None);
                }
                match ext.child {
                    ChildRef::Arena(_) => Ok(None),
                    ChildRef::Hash(hash) => {
                        self.get_persisted_recursive(hash, store, key, offset + ext.nibbles.len())
                    }
                    ChildRef::Inline(bytes) => {
                        self.get_inline_recursive(&bytes, store, key, offset + ext.nibbles.len())
                    }
                }
            }
            MptNode::Branch(branch) => {
                if offset >= key.len() {
                    Ok(branch.value)
                } else {
                    let nibble = key.get_unchecked(offset) as usize;
                    match branch.children[nibble].as_ref() {
                        Some(ChildRef::Arena(_)) => Ok(None),
                        Some(ChildRef::Hash(hash)) => {
                            self.get_persisted_recursive(*hash, store, key, offset + 1)
                        }
                        Some(ChildRef::Inline(bytes)) => {
                            self.get_inline_recursive(bytes, store, key, offset + 1)
                        }
                        None => Ok(None),
                    }
                }
            }
        }
    }

    fn ensure_path_loaded(
        &mut self,
        store: &PersistedTrieStore,
        key: &Nibbles,
    ) -> Result<Option<u32>> {
        let Some(root_idx) = self.mutate_root_for_write(store)? else {
            return Ok(None);
        };
        self.ensure_path_loaded_recursive(store, root_idx, key, 0)?;
        Ok(Some(root_idx))
    }

    fn ensure_delete_ready(
        &mut self,
        store: &PersistedTrieStore,
        key: &Nibbles,
    ) -> Result<Option<u32>> {
        let Some(root_idx) = self.mutate_root_for_write(store)? else {
            return Ok(None);
        };
        self.ensure_delete_ready_recursive(store, root_idx, key, 0)?;
        Ok(Some(root_idx))
    }

    fn mutate_root_for_write(&mut self, store: &PersistedTrieStore) -> Result<Option<u32>> {
        let idx = match self.root.clone() {
            CowRootRef::Empty => return Ok(None),
            CowRootRef::Arena(idx) => idx,
            CowRootRef::Lazy(lazy) => self.mutate_lazy_node_for_write(store, lazy)?,
        };
        self.root = CowRootRef::Arena(idx);
        Ok(Some(idx))
    }

    fn ensure_path_loaded_recursive(
        &mut self,
        store: &PersistedTrieStore,
        idx: u32,
        key: &Nibbles,
        offset: usize,
    ) -> Result<()> {
        if cow_diag_enabled() {
            COW_DIAG_ENSURE_PATH_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        match self.arena.get(idx).clone() {
            MptNode::Leaf(_) => Ok(()),
            MptNode::Extension(ext) => {
                let remaining = key.slice(offset..);
                if remaining.len() < ext.nibbles.len() ||
                    remaining.slice(..ext.nibbles.len()) != ext.nibbles
                {
                    return Ok(());
                }
                let child_idx = self.mutate_child_for_write(store, idx, None, ext.child)?;
                self.ensure_path_loaded_recursive(store, child_idx, key, offset + ext.nibbles.len())
            }
            MptNode::Branch(branch) => {
                if offset >= key.len() {
                    return Ok(());
                }
                let nibble = key.get_unchecked(offset) as usize;
                let Some(child) = branch.children[nibble].clone() else {
                    return Ok(());
                };
                let child_idx = self.mutate_child_for_write(store, idx, Some(nibble), child)?;
                self.ensure_path_loaded_recursive(store, child_idx, key, offset + 1)
            }
        }
    }

    fn ensure_delete_ready_recursive(
        &mut self,
        store: &PersistedTrieStore,
        idx: u32,
        key: &Nibbles,
        offset: usize,
    ) -> Result<()> {
        match self.arena.get(idx).clone() {
            MptNode::Leaf(_) => Ok(()),
            MptNode::Extension(ext) => {
                let remaining = key.slice(offset..);
                if remaining.len() < ext.nibbles.len() ||
                    remaining.slice(..ext.nibbles.len()) != ext.nibbles
                {
                    return Ok(());
                }
                let child_idx = self.mutate_child_for_write(store, idx, None, ext.child)?;
                self.ensure_delete_ready_recursive(
                    store,
                    child_idx,
                    key,
                    offset + ext.nibbles.len(),
                )
            }
            MptNode::Branch(branch) => {
                if offset >= key.len() {
                    if branch.value.is_some() && branch.child_count() == 1 {
                        self.materialize_single_branch_child_if_needed(store, idx)?;
                    }
                    return Ok(());
                }

                let nibble = key.get_unchecked(offset) as usize;
                let Some(child) = branch.children[nibble].clone() else {
                    return Ok(());
                };

                if branch.value.is_none() && branch.child_count() == 2 {
                    self.materialize_other_branch_children(store, idx, nibble)?;
                }

                let child_idx = self.mutate_child_for_write(store, idx, Some(nibble), child)?;
                self.ensure_delete_ready_recursive(store, child_idx, key, offset + 1)
            }
        }
    }

    fn materialize_other_branch_children(
        &mut self,
        store: &PersistedTrieStore,
        branch_idx: u32,
        keep_slot: usize,
    ) -> Result<()> {
        let children = match self.arena.get(branch_idx) {
            MptNode::Branch(branch) => branch.children.clone(),
            _ => return Ok(()),
        };

        for (slot, child) in children.into_iter().enumerate() {
            if slot == keep_slot {
                continue;
            }
            match child {
                Some(ChildRef::Arena(_)) | None => {}
                Some(other) => {
                    let _ = self.mutate_child_for_write(store, branch_idx, Some(slot), other)?;
                }
            }
        }
        Ok(())
    }

    fn materialize_single_branch_child_if_needed(
        &mut self,
        store: &PersistedTrieStore,
        branch_idx: u32,
    ) -> Result<()> {
        let children = match self.arena.get(branch_idx) {
            MptNode::Branch(branch) => branch.children.clone(),
            _ => return Ok(()),
        };

        for (slot, child) in children.into_iter().enumerate() {
            match child {
                Some(ChildRef::Arena(_)) | None => {}
                Some(other) => {
                    let _ = self.mutate_child_for_write(store, branch_idx, Some(slot), other)?;
                    break;
                }
            }
        }
        Ok(())
    }

    fn mutate_child_for_write(
        &mut self,
        store: &PersistedTrieStore,
        parent_idx: u32,
        branch_slot: Option<usize>,
        child: ChildRef,
    ) -> Result<u32> {
        let edge = pending_edge(branch_slot)?;
        let child_idx = match self.pending_lazy_children.remove(&(parent_idx, edge)) {
            Some(CowChildRef::Arena(idx)) => idx,
            Some(CowChildRef::Lazy(lazy)) => self.mutate_lazy_node_for_write(store, lazy)?,
            None => match child {
                ChildRef::Arena(idx) => idx,
                other => self.mutate_lazy_node_for_write(store, child_ref_to_lazy(other))?,
            },
        };

        match self.arena.get_mut(parent_idx) {
            MptNode::Extension(ext) => ext.child = ChildRef::Arena(child_idx),
            MptNode::Branch(branch) => {
                let slot = branch_slot.expect("branch child materialization requires slot");
                branch.children[slot] = Some(ChildRef::Arena(child_idx));
            }
            MptNode::Leaf(_) => unreachable!("leaf cannot own child ref"),
        }
        Ok(child_idx)
    }

    fn mutate_persisted_root(&mut self, store: &PersistedTrieStore, root: B256) -> Result<u32> {
        let rlp = store
            .get_node(root)?
            .ok_or_else(|| MptDbError::Other(format!("child node not found: {root}")))?;
        let node =
            decode_node(&rlp).map_err(|e| MptDbError::Other(format!("decode child node: {e}")))?;
        let idx = self.arena.alloc_clean(node);
        self.arena.set_hash(idx, root);
        Ok(idx)
    }

    fn mutate_lazy_node_for_write(
        &mut self,
        store: &PersistedTrieStore,
        lazy: CowLazyNodeRef,
    ) -> Result<u32> {
        match lazy {
            CowLazyNodeRef::Persisted(root) => self.mutate_persisted_root(store, root),
            CowLazyNodeRef::Inline(rlp) => self.materialize_inline_node(&rlp),
            CowLazyNodeRef::Segment(node_ref) => self.mutate_segment_node(node_ref),
        }
    }

    fn materialize_inline_node(&mut self, rlp: &[u8]) -> Result<u32> {
        let node =
            decode_node(rlp).map_err(|e| MptDbError::Other(format!("decode inline child: {e}")))?;
        Ok(self.arena.alloc_clean(node))
    }

    fn mutate_segment_node(&mut self, node_ref: SegmentNodeRef) -> Result<u32> {
        if cow_diag_enabled() {
            COW_DIAG_MUTATE_SEGMENT_NODE_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        let reader = StorageTrieSegmentReader::open_shared_page(
            node_ref.page_lease(),
            node_ref.page_lease().root(),
            node_ref.page_lease().root_record_off(),
        )?;
        let view = reader.view_node(node_ref.seg_idx())?;
        let mut deferred_children = Vec::new();
        let node = match view.kind {
            SegmentNodeKind::Leaf { nibbles, value } => MptNode::Leaf(LeafNode {
                nibbles: Nibbles::from_nibbles(nibbles),
                value: value.to_vec(),
            }),
            SegmentNodeKind::Extension { nibbles, child } => {
                if let Some(seg_idx) = child.target_idx {
                    let hash = reader.view_node(seg_idx)?.hash;
                    deferred_children.push((
                        PendingCowEdge::Extension,
                        SegmentNodeRef::new(Arc::clone(node_ref.page_lease()), seg_idx, hash),
                    ));
                }
                MptNode::Extension(ExtensionNode {
                    nibbles: Nibbles::from_nibbles(nibbles),
                    child: segment_child_to_ref(child.embed)?,
                })
            }
            SegmentNodeKind::Branch { value, children, .. } => {
                let mut branch = BranchNode::new();
                branch.value = value.map(|value| value.to_vec());
                for child in children.iter() {
                    let child = child?;
                    if let Some(seg_idx) = child.target_idx {
                        let hash = reader.view_node(seg_idx)?.hash;
                        deferred_children.push((
                            PendingCowEdge::Branch(child.slot),
                            SegmentNodeRef::new(Arc::clone(node_ref.page_lease()), seg_idx, hash),
                        ));
                    }
                    branch.children[child.slot as usize] = Some(segment_child_to_ref(child.embed)?);
                }
                MptNode::Branch(branch)
            }
        };
        let idx = self.arena.alloc_clean(node);
        for (edge, child_ref) in deferred_children {
            self.pending_lazy_children
                .insert((idx, edge), CowChildRef::Lazy(CowLazyNodeRef::Segment(child_ref)));
        }
        if let Some(hash) = node_ref.hash() {
            self.arena.set_hash(idx, hash);
        }
        Ok(idx)
    }

    #[cfg(test)]
    fn materialize_segment_node(&mut self, node_ref: SegmentNodeRef) -> Result<u32> {
        self.mutate_segment_node(node_ref)
    }

    fn materialize_root_subtree(
        &mut self,
        store: &PersistedTrieStore,
        root: CowRootRef,
    ) -> Result<Option<u32>> {
        let idx = match root {
            CowRootRef::Empty => return Ok(None),
            CowRootRef::Arena(idx) => idx,
            CowRootRef::Lazy(lazy) => self.materialize_lazy_subtree(store, lazy)?,
        };
        Ok(Some(idx))
    }

    fn materialize_lazy_subtree(
        &mut self,
        store: &PersistedTrieStore,
        lazy: CowLazyNodeRef,
    ) -> Result<u32> {
        match lazy {
            CowLazyNodeRef::Segment(node_ref) => self.materialize_segment_lazy_subtree(node_ref),
            CowLazyNodeRef::Persisted(root) => self.materialize_persisted_lazy_subtree(store, root),
            CowLazyNodeRef::Inline(_) => {
                return Err(MptDbError::Other("root cannot be inline lazy node".to_string()))
            }
        }
    }

    fn prune_pending_lazy_children(&mut self) {
        if self.pending_lazy_children.is_empty() {
            return;
        }

        let mut reachable = vec![false; self.arena.len()];
        let mut stack = Vec::new();
        if let CowRootRef::Arena(root_idx) = self.root {
            stack.push(root_idx);
        }

        while let Some(idx) = stack.pop() {
            let slot = idx as usize;
            if slot >= reachable.len() || reachable[slot] {
                continue;
            }
            reachable[slot] = true;

            match self.arena.get(idx) {
                MptNode::Leaf(_) => {}
                MptNode::Extension(ext) => {
                    if let ChildRef::Arena(child_idx) = ext.child {
                        stack.push(child_idx);
                    }
                }
                MptNode::Branch(branch) => {
                    for child in branch.children.iter().flatten() {
                        if let ChildRef::Arena(child_idx) = child {
                            stack.push(*child_idx);
                        }
                    }
                }
            }
        }

        self.pending_lazy_children.retain(|(idx, edge), _| {
            let slot = *idx as usize;
            if slot >= reachable.len() || !reachable[slot] {
                return false;
            }

            match (self.arena.get(*idx), edge) {
                (MptNode::Extension(ext), PendingCowEdge::Extension) => {
                    !matches!(ext.child, ChildRef::Arena(_))
                }
                (MptNode::Branch(branch), PendingCowEdge::Branch(slot)) => branch.children
                    [*slot as usize]
                    .as_ref()
                    .is_some_and(|child| !matches!(child, ChildRef::Arena(_))),
                _ => false,
            }
        });
    }

    fn materialize_segment_lazy_subtree(&mut self, node_ref: SegmentNodeRef) -> Result<u32> {
        let reader = StorageTrieSegmentReader::open_shared_page(
            node_ref.page_lease(),
            node_ref.page_lease().root(),
            node_ref.page_lease().root_record_off(),
        )?;
        let view = reader.view_node(node_ref.seg_idx())?;
        let idx = match view.kind {
            SegmentNodeKind::Leaf { nibbles, value } => {
                self.arena.alloc_clean(MptNode::Leaf(LeafNode {
                    nibbles: Nibbles::from_nibbles(nibbles),
                    value: value.to_vec(),
                }))
            }
            SegmentNodeKind::Extension { nibbles, child } => {
                let child_ref = match child.target_idx {
                    Some(seg_idx) => {
                        let hash = reader.view_node(seg_idx)?.hash;
                        let child_idx = self.materialize_segment_lazy_subtree(
                            SegmentNodeRef::new(Arc::clone(node_ref.page_lease()), seg_idx, hash),
                        )?;
                        ChildRef::Arena(child_idx)
                    }
                    None => segment_child_to_ref(child.embed)?,
                };
                self.arena.alloc_clean(MptNode::Extension(ExtensionNode {
                    nibbles: Nibbles::from_nibbles(nibbles),
                    child: child_ref,
                }))
            }
            SegmentNodeKind::Branch { value, children, .. } => {
                let mut branch = BranchNode::new();
                branch.value = value.map(|value| value.to_vec());
                for child in children.iter() {
                    let child = child?;
                    branch.children[child.slot as usize] = Some(match child.target_idx {
                        Some(seg_idx) => {
                            let hash = reader.view_node(seg_idx)?.hash;
                            let child_idx =
                                self.materialize_segment_lazy_subtree(SegmentNodeRef::new(
                                    Arc::clone(node_ref.page_lease()),
                                    seg_idx,
                                    hash,
                                ))?;
                            ChildRef::Arena(child_idx)
                        }
                        None => segment_child_to_ref(child.embed)?,
                    });
                }
                self.arena.alloc_clean(MptNode::Branch(branch))
            }
        };
        if let Some(hash) = node_ref.hash() {
            self.arena.set_hash(idx, hash);
        }
        Ok(idx)
    }

    fn materialize_persisted_lazy_subtree(
        &mut self,
        store: &PersistedTrieStore,
        root: B256,
    ) -> Result<u32> {
        let rlp = store
            .get_node(root)?
            .ok_or_else(|| MptDbError::Other(format!("child node not found: {root}")))?;
        let node =
            decode_node(&rlp).map_err(|e| MptDbError::Other(format!("decode child node: {e}")))?;
        let idx = match node {
            MptNode::Leaf(leaf) => self.arena.alloc_clean(MptNode::Leaf(leaf)),
            MptNode::Extension(ext) => {
                let child_idx =
                    materialize_lazy_child(store, &mut self.arena, child_ref_to_lazy(ext.child))?;
                self.arena.alloc_clean(MptNode::Extension(ExtensionNode {
                    nibbles: ext.nibbles,
                    child: ChildRef::Arena(child_idx),
                }))
            }
            MptNode::Branch(branch) => {
                let mut children: [Option<ChildRef>; 16] = std::array::from_fn(|_| None);
                for (slot, child) in branch.children.into_iter().enumerate() {
                    if let Some(child) = child {
                        let child_idx = materialize_lazy_child(
                            store,
                            &mut self.arena,
                            child_ref_to_lazy(child),
                        )?;
                        children[slot] = Some(ChildRef::Arena(child_idx));
                    }
                }
                self.arena
                    .alloc_clean(MptNode::Branch(BranchNode { children, value: branch.value }))
            }
        };
        self.arena.set_hash(idx, root);
        Ok(idx)
    }
}

fn materialize_lazy_child(
    store: &PersistedTrieStore,
    arena: &mut MutableTrieArena,
    lazy: CowLazyNodeRef,
) -> Result<u32> {
    match lazy {
        CowLazyNodeRef::Persisted(hash) => {
            let rlp = store
                .get_node(hash)?
                .ok_or_else(|| MptDbError::Other(format!("child node not found: {hash}")))?;
            let node = decode_node(&rlp)
                .map_err(|e| MptDbError::Other(format!("decode child node: {e}")))?;
            let idx = match node {
                MptNode::Leaf(leaf) => arena.alloc_clean(MptNode::Leaf(leaf)),
                MptNode::Extension(ext) => {
                    let child_idx =
                        materialize_lazy_child(store, arena, child_ref_to_lazy(ext.child))?;
                    arena.alloc_clean(MptNode::Extension(ExtensionNode {
                        nibbles: ext.nibbles,
                        child: ChildRef::Arena(child_idx),
                    }))
                }
                MptNode::Branch(branch) => {
                    let mut children: [Option<ChildRef>; 16] = std::array::from_fn(|_| None);
                    for (slot, child) in branch.children.into_iter().enumerate() {
                        if let Some(child) = child {
                            let child_idx =
                                materialize_lazy_child(store, arena, child_ref_to_lazy(child))?;
                            children[slot] = Some(ChildRef::Arena(child_idx));
                        }
                    }
                    arena.alloc_clean(MptNode::Branch(BranchNode { children, value: branch.value }))
                }
            };
            arena.set_hash(idx, hash);
            Ok(idx)
        }
        CowLazyNodeRef::Inline(rlp) => {
            let node = decode_node(&rlp)
                .map_err(|e| MptDbError::Other(format!("decode inline child: {e}")))?;
            match node {
                MptNode::Leaf(leaf) => Ok(arena.alloc_clean(MptNode::Leaf(leaf))),
                MptNode::Extension(ext) => {
                    let child_idx =
                        materialize_lazy_child(store, arena, child_ref_to_lazy(ext.child))?;
                    Ok(arena.alloc_clean(MptNode::Extension(ExtensionNode {
                        nibbles: ext.nibbles,
                        child: ChildRef::Arena(child_idx),
                    })))
                }
                MptNode::Branch(branch) => {
                    let mut children: [Option<ChildRef>; 16] = std::array::from_fn(|_| None);
                    for (slot, child) in branch.children.into_iter().enumerate() {
                        if let Some(child) = child {
                            let child_idx =
                                materialize_lazy_child(store, arena, child_ref_to_lazy(child))?;
                            children[slot] = Some(ChildRef::Arena(child_idx));
                        }
                    }
                    Ok(arena
                        .alloc_clean(MptNode::Branch(BranchNode { children, value: branch.value })))
                }
            }
        }
        CowLazyNodeRef::Segment(node_ref) => {
            let mut cow = StorageTrieCow {
                root: CowRootRef::Empty,
                arena: std::mem::take(arena),
                pending_lazy_children: HashMap::new(),
            };
            let idx = cow.materialize_segment_lazy_subtree(node_ref)?;
            *arena = cow.arena;
            Ok(idx)
        }
    }
}

fn segment_child_to_ref(embed: SegmentChildEmbedRef<'_>) -> Result<ChildRef> {
    Ok(match embed {
        SegmentChildEmbedRef::None => {
            return Err(MptDbError::Other("missing child embed".to_string()));
        }
        SegmentChildEmbedRef::Hash(hash) => ChildRef::Hash(hash),
        SegmentChildEmbedRef::Inline(bytes) => ChildRef::Inline(bytes.to_vec()),
    })
}

fn child_ref_to_lazy(child: ChildRef) -> CowLazyNodeRef {
    match child {
        ChildRef::Arena(_) => unreachable!("arena child is not lazy"),
        ChildRef::Hash(hash) => CowLazyNodeRef::Persisted(hash),
        ChildRef::Inline(rlp) => CowLazyNodeRef::Inline(rlp),
    }
}

fn pending_edge(branch_slot: Option<usize>) -> Result<PendingCowEdge> {
    match branch_slot {
        Some(slot) => Ok(PendingCowEdge::Branch(
            u8::try_from(slot)
                .map_err(|_| MptDbError::Other(format!("branch slot out of range: {slot}")))?,
        )),
        None => Ok(PendingCowEdge::Extension),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memmap2::MmapOptions;
    use std::{fs::File, io::Write};
    use tempfile::{NamedTempFile, TempDir};

    use crate::mpt::{
        persisted::PersistedTrieStore,
        segment::{MappedSegmentPage, StorageTrieSegment},
    };

    #[test]
    fn cow_empty_starts_empty() {
        let cow = StorageTrieCow::empty();
        assert!(matches!(cow.root_ref(), CowRootRef::Empty));
        assert!(cow.arena().is_empty());
    }

    #[test]
    fn cow_from_persisted_root_keeps_root_hash() {
        let root = B256::with_last_byte(0x11);
        let cow = StorageTrieCow::from_persisted_root(root);
        assert!(matches!(
            cow.root_ref(),
            CowRootRef::Lazy(CowLazyNodeRef::Persisted(value)) if *value == root
        ));
    }

    #[test]
    fn cow_materialize_persisted_root_seeds_hash_cache() {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();

        let key = Nibbles::unpack(B256::with_last_byte(0x31));
        let mut tree = MptTree::new();
        tree.insert(&key, b"one".to_vec());
        let (root, blobs) = tree.root_hash_and_dirty_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let mut cow = StorageTrieCow::from_persisted_root(root);
        let idx = cow.materialize_lazy_subtree(&store, CowLazyNodeRef::Persisted(root)).unwrap();
        assert_eq!(cow.arena_hash_cache()[idx as usize], Some(root));
    }

    #[test]
    fn cow_from_segment_page_uses_runtime_root_ref() {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(0x21));
        tree.insert(&key, b"one".to_vec());
        let root = tree.root_hash();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));
        let cow = StorageTrieCow::from_segment_page(Arc::clone(&lease));
        match cow.root_ref() {
            CowRootRef::Lazy(CowLazyNodeRef::Segment(root)) => {
                assert_eq!(root.page_lease().root(), lease.root());
                assert_eq!(root.hash(), Some(lease.root()));
            }
            _ => panic!("expected segment root"),
        }
    }

    #[test]
    fn cow_get_reads_from_segment_page_without_mutation() {
        let key = Nibbles::from_nibbles(&[1, 2]);
        let mut tree = MptTree::new();
        tree.insert(&key, b"one".to_vec());
        let root = tree.root_hash();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let cow = StorageTrieCow::from_segment_page(lease);
        assert_eq!(cow.get(&store, &key).unwrap(), Some(b"one".to_vec()));
    }

    #[test]
    fn cow_get_reads_from_persisted_root_without_materialize() {
        let key = Nibbles::from_nibbles(&[4, 5]);
        let mut tree = MptTree::new();
        tree.insert(&key, b"two".to_vec());
        let (root, blobs) = tree.root_hash_and_dirty_blobs();

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        store.persist_batch(&blobs, true).unwrap();

        let cow = StorageTrieCow::from_persisted_root(root);
        assert_eq!(cow.get(&store, &key).unwrap(), Some(b"two".to_vec()));
    }

    #[test]
    fn cow_materialize_segment_node_tracks_pending_lazy_children() {
        let mut base = MptTree::new();
        let key1 = Nibbles::from_nibbles(&[1]);
        let key2 = Nibbles::from_nibbles(&[2]);
        base.insert(&key1, vec![0xaa; 64]);
        base.insert(&key2, vec![0xbb; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let mut cow = StorageTrieCow::from_segment_page(lease);
        let root_ref = match cow.root_ref() {
            CowRootRef::Lazy(CowLazyNodeRef::Segment(root_ref)) => root_ref.clone(),
            _ => panic!("expected segment root"),
        };
        let idx = cow.materialize_segment_node(root_ref).unwrap();

        assert!(cow.pending_lazy_children.contains_key(&(idx, PendingCowEdge::Branch(1))));
        assert!(cow.pending_lazy_children.contains_key(&(idx, PendingCowEdge::Branch(2))));
        assert!(matches!(
            cow.pending_lazy_children.get(&(idx, PendingCowEdge::Branch(1))),
            Some(CowChildRef::Lazy(CowLazyNodeRef::Segment(_)))
        ));
    }

    #[test]
    fn cow_segment_pending_child_allows_mutation_without_persisted_lookup() {
        let mut base = MptTree::new();
        let key1 = Nibbles::from_nibbles(&[1]);
        let key2 = Nibbles::from_nibbles(&[2]);
        base.insert(&key1, vec![0xaa; 64]);
        base.insert(&key2, vec![0xbb; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_change(&store, &key1, Some(vec![0xcc; 64])).unwrap();

        let overlay = cow.into_overlay_materialized(&store).unwrap();
        base.insert(&key1, vec![0xcc; 64]);
        assert_eq!(overlay.root_hash_and_dirty_blobs().0, base.root_hash());
    }

    #[test]
    fn batched_apply_roundtrip_keeps_segment_paths_without_persisted_nodes() {
        let mut base = MptTree::new();
        let key1 = Nibbles::from_nibbles(&[1]);
        let key2 = Nibbles::from_nibbles(&[2]);
        base.insert(&key1, vec![0xaa; 64]);
        base.insert(&key2, vec![0xbb; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        // Persisted store intentionally empty: wal_first must not depend on
        // persisted-node fallback for untouched subtree reads.
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let mut cow = StorageTrieCow::from_segment_page(lease);

        let change1 = StorageChange {
            hashed_slot: B256::with_last_byte(1),
            slot_key: key1.clone(),
            value: alloy_primitives::U256::from(0xcc_u64),
            encoded_value: Some(vec![0xcc; 64]),
        };
        cow.apply_changes_batched(&store, &[change1]).unwrap();

        let (root1, mut cow) = cow.root_hash_only(&store).unwrap();
        cow.clear_dirty();
        let mut cached = cow.into_snapshot_cached(root1, None, true).unwrap();

        // Next block touches a different key; this used to fail with
        // "child node not found" when batched preload introduced hash-only
        // siblings that required persisted fallback.
        cached.apply_change(&store, &key2, Some(vec![0xdd; 64])).unwrap();
    }

    /// Extension child provenance is preserved across blocks in wal_first mode.
    ///
    /// Setup: base segment contains an extension node (requires at least two
    /// keys sharing a long prefix so the trie produces an extension rather
    /// than a branch at the root).
    ///
    /// Block 1: update key1 (path goes THROUGH the extension → child becomes
    ///          Arena after trace_paths, extension child provenance not needed).
    ///
    /// Block 2: insert key_new (path DIVERGES from the extension at nibble 2).
    ///          trace_paths materialises the extension with inserted=true but
    ///          returns early, leaving ext.child as ChildRef::Hash.
    ///          The lazy sibling fix must have recorded the SegmentNodeRef so
    ///          that the extension split in apply_change can proceed without
    ///          hitting the empty persisted store.
    #[test]
    fn batched_apply_extension_child_provenance_without_persisted_lookup() {
        // Trie with two keys sharing a long prefix → produces an extension.
        // key_a = [1,2,3,4,5], key_b = [1,2,3,4,7]:
        //   extension [1,2,3,4] → branch { slot 5: leaf, slot 7: leaf }
        let mut base = MptTree::new();
        let key_a = Nibbles::from_nibbles(&[1, 2, 3, 4, 5]);
        let key_b = Nibbles::from_nibbles(&[1, 2, 3, 4, 7]);
        let key_new = Nibbles::from_nibbles(&[1, 2, 8, 9]); // diverges from ext at nibble-idx 2
        base.insert(&key_a, vec![0xaa; 64]);
        base.insert(&key_b, vec![0xbb; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        // Persisted store intentionally empty: wal_first must not use it.
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();

        // Block 1: update key_a (matches extension fully → child Arena after trace).
        let change_a = StorageChange {
            hashed_slot: B256::with_last_byte(1),
            slot_key: key_a.clone(),
            value: alloy_primitives::U256::from(0xcc_u64),
            encoded_value: Some(vec![0xcc; 64]),
        };
        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_changes_batched(&store, &[change_a]).unwrap();
        let (root1, mut cow) = cow.root_hash_only(&store).unwrap();
        cow.clear_dirty();
        // Simulate wal_first async L2 snapshot (no published segment yet).
        let mut cached = cow.into_snapshot_cached(root1, None, true).unwrap();

        // Block 2: insert key_new which diverges from extension [1,2,3,4] at
        // nibble index 2.  trace_paths materialises the extension node but
        // returns early (nibble 8 != 3), leaving ext.child as ChildRef::Hash.
        // The extension lazy-sibling fix must have captured the SegmentNodeRef
        // so that this apply_change succeeds without touching the empty store.
        cached.apply_change(&store, &key_new, Some(vec![0xdd; 64])).unwrap();
        let (root2, _) = cached.root_hash_only(&store).unwrap();

        // Verify against a reference trie built from scratch.
        let mut reference = MptTree::new();
        reference.insert(&key_a, vec![0xcc; 64]);
        reference.insert(&key_b, vec![0xbb; 64]);
        reference.insert(&key_new, vec![0xdd; 64]);
        assert_eq!(root2, reference.root_hash());
    }

    #[test]
    fn cow_prunes_pending_lazy_children_when_root_changes() {
        let mut base = MptTree::new();
        let key1 = Nibbles::from_nibbles(&[1]);
        let key2 = Nibbles::from_nibbles(&[2]);
        base.insert(&key1, vec![0xaa; 64]);
        base.insert(&key2, vec![0xbb; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let mut cow = StorageTrieCow::from_segment_page(lease);
        let root_ref = match cow.root_ref() {
            CowRootRef::Lazy(CowLazyNodeRef::Segment(root_ref)) => root_ref.clone(),
            _ => panic!("expected segment root"),
        };
        let idx = cow.materialize_segment_node(root_ref).unwrap();
        cow.root = CowRootRef::Arena(idx);
        assert!(!cow.pending_lazy_children.is_empty());

        cow.root = CowRootRef::Empty;
        cow.prune_pending_lazy_children();
        assert!(cow.pending_lazy_children.is_empty());
    }

    #[test]
    fn cow_mixed_segment_hash_inline_recompute_matches_materialized_tree() {
        let key_inline = Nibbles::from_nibbles(&[1]);
        let key_hash = Nibbles::from_nibbles(&[2]);
        let key_mutate = Nibbles::from_nibbles(&[3]);

        let mut base = MptTree::new();
        base.insert(&key_inline, vec![0x11]);
        base.insert(&key_hash, vec![0x22; 64]);
        base.insert(&key_mutate, vec![0x33; 64]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_change(&store, &key_mutate, Some(vec![0x44; 64])).unwrap();
        let (cow_root, _, _) = cow.root_hash_and_dirty_blobs(&store).unwrap();

        base.insert(&key_mutate, vec![0x44; 64]);
        assert_eq!(cow_root, base.root_hash());
    }

    #[test]
    fn cow_batched_changes_latest_wins_same_slot() {
        let key = Nibbles::from_nibbles(&[4]);

        let mut base = MptTree::new();
        base.insert(&key, vec![0x01]);
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let changes = vec![
            StorageChange {
                hashed_slot: B256::with_last_byte(0x04),
                slot_key: key.clone(),
                value: alloy_primitives::U256::from(2u64),
                encoded_value: Some(vec![0x02]),
            },
            StorageChange {
                hashed_slot: B256::with_last_byte(0x04),
                slot_key: key.clone(),
                value: alloy_primitives::U256::from(3u64),
                encoded_value: Some(vec![0x03]),
            },
            StorageChange {
                hashed_slot: B256::with_last_byte(0x04),
                slot_key: key.clone(),
                value: alloy_primitives::U256::ZERO,
                encoded_value: None,
            },
        ];

        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_changes_batched(&store, &changes).unwrap();
        let (cow_root, _, _) = cow.root_hash_and_dirty_blobs(&store).unwrap();

        base.delete(&key);
        assert_eq!(cow_root, base.root_hash());
    }

    #[test]
    fn cow_segment_insert_matches_full_tree() {
        let mut base = MptTree::new();
        let key1 = Nibbles::unpack(B256::with_last_byte(0x11));
        let key2 = Nibbles::unpack(B256::with_last_byte(0x22));
        base.insert(&key1, b"one".to_vec());
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_change(&store, &key2, Some(b"two".to_vec())).unwrap();
        let overlay = cow.into_overlay_materialized(&store).unwrap();

        base.insert(&key2, b"two".to_vec());
        assert_eq!(overlay.root_hash_and_dirty_blobs().0, base.root_hash());
    }

    #[test]
    fn cow_persisted_delete_matches_full_tree() {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();

        let key1 = Nibbles::unpack(B256::with_last_byte(0x31));
        let key2 = Nibbles::unpack(B256::with_last_byte(0x32));
        let mut base = MptTree::new();
        base.insert(&key1, b"one".to_vec());
        base.insert(&key2, b"two".to_vec());
        let (root, blobs) = base.root_hash_and_dirty_blobs();
        store.persist_batch(&blobs, true).unwrap();

        let mut cow = StorageTrieCow::from_persisted_root(root);
        cow.apply_change(&store, &key1, None).unwrap();
        let overlay = cow.into_overlay_materialized(&store).unwrap();

        base.delete(&key1);
        assert_eq!(overlay.root_hash_and_dirty_blobs().0, base.root_hash());
    }

    #[test]
    fn cow_segment_delete_branch_collapse_matches_full_tree() {
        let mut base = MptTree::new();
        let key1 = Nibbles::unpack(B256::with_last_byte(0x51));
        let key2 = Nibbles::unpack(B256::with_last_byte(0x52));
        base.insert(&key1, b"one".to_vec());
        base.insert(&key2, b"two".to_vec());
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        let mut cow = StorageTrieCow::from_segment_page(lease);
        cow.apply_change(&store, &key1, None).unwrap();
        let overlay = cow.into_overlay_materialized(&store).unwrap();

        base.delete(&key1);
        assert_eq!(overlay.root_hash_and_dirty_blobs().0, base.root_hash());
    }

    #[test]
    fn cow_batched_changes_match_sequential() {
        let mut base = MptTree::new();
        let key1 = Nibbles::unpack(B256::with_last_byte(0x41));
        let key2 = Nibbles::unpack(B256::with_last_byte(0x42));
        let key3 = Nibbles::unpack(B256::with_last_byte(0x43));
        base.insert(&key1, b"one".to_vec());
        let root = base.root_hash();
        let segment = StorageTrieSegment::from_tree(&base, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));

        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();

        let changes = vec![
            StorageChange {
                hashed_slot: B256::with_last_byte(0x43),
                slot_key: key3.clone(),
                value: alloy_primitives::U256::from(3u64),
                encoded_value: Some(alloy_rlp::encode(3u64)),
            },
            StorageChange {
                hashed_slot: B256::with_last_byte(0x41),
                slot_key: key1.clone(),
                value: alloy_primitives::U256::ZERO,
                encoded_value: None,
            },
            StorageChange {
                hashed_slot: B256::with_last_byte(0x42),
                slot_key: key2.clone(),
                value: alloy_primitives::U256::from(2u64),
                encoded_value: Some(alloy_rlp::encode(2u64)),
            },
        ];

        let mut batched = StorageTrieCow::from_segment_page(Arc::clone(&lease));
        batched.apply_changes_batched(&store, &changes).unwrap();
        let batched_overlay = batched.into_overlay_materialized(&store).unwrap();

        let mut sequential = StorageTrieCow::from_segment_page(lease);
        for change in &changes {
            let value = if change.value == alloy_primitives::U256::ZERO {
                None
            } else {
                change.encoded_value.clone()
            };
            sequential.apply_change(&store, &change.slot_key, value).unwrap();
        }
        let sequential_overlay = sequential.into_overlay_materialized(&store).unwrap();

        assert_eq!(
            batched_overlay.root_hash_and_dirty_blobs().0,
            sequential_overlay.root_hash_and_dirty_blobs().0
        );
    }
}
