use alloy_trie::Nibbles;

use super::{
    arena::MutableTrieArena, segment::StoragePathTrace, storage_recompute, tree::MptTree, tree_algo,
};

#[derive(Clone)]
pub struct StorageOverlay {
    arena: MutableTrieArena,
    root: Option<u32>,
}

impl StorageOverlay {
    pub fn empty() -> Self {
        Self { arena: MutableTrieArena::new(), root: None }
    }

    pub fn from_trace(trace: StoragePathTrace) -> Self {
        let (arena, root, _lazy_siblings) = trace.into_parts();
        Self { arena, root }
    }

    pub fn from_tree(tree: MptTree) -> Self {
        Self { arena: tree.arena, root: tree.root }
    }

    pub fn apply_change(
        &mut self,
        _hashed_slot: alloy_primitives::B256,
        key: Nibbles,
        value: Option<Vec<u8>>,
    ) {
        match value {
            Some(value) => {
                tree_algo::note_slot_insert();
                let new_root =
                    tree_algo::insert_recursive(&mut self.arena, self.root, &key, 0, value);
                self.root = Some(new_root);
            }
            None => {
                tree_algo::note_slot_delete();
                let (_deleted, new_root) =
                    tree_algo::delete_recursive(&mut self.arena, self.root, &key, 0);
                self.root = new_root;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn into_tree(self) -> MptTree {
        MptTree { arena: self.arena, root: self.root }
    }

    pub fn root_hash_and_dirty_blobs(
        mut self,
    ) -> (alloy_primitives::B256, Vec<(alloy_primitives::B256, Vec<u8>)>, StorageOverlay) {
        let result = storage_recompute::recompute(&mut self.arena, self.root);
        (result.root, result.dirty_blobs, self)
    }

    pub fn clear_dirty(&mut self) {
        self.arena.clear_all_dirty();
    }

    pub fn arena_nodes(&self) -> Vec<super::node::MptNode> {
        self.arena.collect_all_nodes()
    }

    pub fn arena_hash_cache(&self) -> Vec<Option<alloy_primitives::B256>> {
        let len = self.arena.len();
        (0..len).map(|i| self.arena.get_hash(i as u32)).collect()
    }

    pub fn root_index(&self) -> Option<u32> {
        self.root
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};
    use alloy_trie::Nibbles;

    use super::StorageOverlay;
    use crate::mpt::tree::MptTree;

    #[test]
    fn overlay_empty_roundtrip() {
        let overlay = StorageOverlay::empty();
        assert!(overlay.is_empty());
        assert!(overlay.into_tree().is_empty());
    }

    #[test]
    fn overlay_latest_wins_for_same_slot() {
        let key = Nibbles::unpack(B256::with_last_byte(0x11));
        let slot = B256::with_last_byte(0xaa);
        let mut overlay = StorageOverlay::empty();
        overlay.apply_change(slot, key.clone(), Some(alloy_rlp::encode(U256::from(1u64))));
        overlay.apply_change(slot, key.clone(), Some(alloy_rlp::encode(U256::from(2u64))));
        let tree = overlay.into_tree();
        assert_eq!(tree.get(&key), Some(alloy_rlp::encode(U256::from(2u64)).as_slice()));
    }

    #[test]
    fn overlay_from_trace_applies_changes() {
        let key = Nibbles::unpack(B256::with_last_byte(0x01));
        let mut base = MptTree::new();
        base.insert(&key, vec![0x01]);
        let root = base.root_hash();
        let seg = crate::mpt::segment::StorageTrieSegment::from_tree(&base, root).unwrap();
        let reader =
            crate::mpt::segment::StorageTrieSegmentReader::open(seg.as_bytes(), root).unwrap();
        let trace = reader.trace_touched_paths(std::slice::from_ref(&key)).unwrap();

        let mut overlay = StorageOverlay::from_trace(trace);
        overlay.apply_change(B256::with_last_byte(0x01), key.clone(), Some(vec![0x02]));
        let tree = overlay.into_tree();
        assert_eq!(tree.get(&key), Some(&[0x02][..]));
    }
}
