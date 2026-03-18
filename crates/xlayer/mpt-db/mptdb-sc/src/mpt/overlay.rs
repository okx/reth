use alloy_trie::Nibbles;

use super::{segment::StoragePathTrace, tree::MptTree};

pub struct StorageOverlay {
    tree: MptTree,
}

impl StorageOverlay {
    pub fn empty() -> Self {
        Self { tree: MptTree::new() }
    }

    pub fn from_trace(trace: StoragePathTrace) -> Self {
        Self { tree: trace.into_tree() }
    }

    pub fn apply_change(
        &mut self,
        _hashed_slot: alloy_primitives::B256,
        key: Nibbles,
        value: Option<Vec<u8>>,
    ) {
        match value {
            Some(value) => self.tree.insert(&key, value),
            None => {
                self.tree.delete(&key);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn into_tree(self) -> MptTree {
        self.tree
    }

    pub fn root_hash_and_dirty_blobs(
        mut self,
    ) -> (alloy_primitives::B256, Vec<(alloy_primitives::B256, Vec<u8>)>, MptTree) {
        let (root, blobs) = self.tree.root_hash_and_dirty_blobs();
        (root, blobs, self.tree)
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
