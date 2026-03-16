use alloy_primitives::B256;
use alloy_trie::Nibbles;

/// MPT node, stored in an arena.
#[derive(Clone, Debug)]
pub enum MptNode {
    /// 16-way branch with optional value.
    Branch(BranchNode),
    /// Shared prefix compression.
    Extension(ExtensionNode),
    /// Leaf: remaining nibbles + value.
    Leaf(LeafNode),
}

#[derive(Clone, Debug)]
pub struct BranchNode {
    pub children: [Option<ChildRef>; 16],
    pub value: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct ExtensionNode {
    pub nibbles: Nibbles,
    pub child: ChildRef,
}

#[derive(Clone, Debug)]
pub struct LeafNode {
    pub nibbles: Nibbles,
    pub value: Vec<u8>,
}

/// Child node reference: three forms.
/// In Phase 1 live tree, only Arena(u32) is used.
/// Hash and Inline are produced by decode_node (Phase 5 disk loading).
#[derive(Clone, Debug)]
pub enum ChildRef {
    /// Node in the current mutable arena.
    Arena(u32),
    /// On-disk node, only hash known (32-byte child in RLP).
    Hash(B256),
    /// RLP < 32 bytes, inlined directly.
    Inline(Vec<u8>),
}

impl BranchNode {
    pub fn new() -> Self {
        Self { children: std::array::from_fn(|_| None), value: None }
    }

    /// Count of non-None children.
    pub fn child_count(&self) -> usize {
        self.children.iter().filter(|c| c.is_some()).count()
    }

    /// If exactly one child exists, return its (index, reference).
    pub fn single_child(&self) -> Option<(u8, &ChildRef)> {
        let mut result = None;
        let mut count = 0;
        for (i, c) in self.children.iter().enumerate() {
            if let Some(child) = c {
                count += 1;
                if count > 1 {
                    return None;
                }
                result = Some((i as u8, child));
            }
        }
        result
    }
}

impl Default for BranchNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MptNode {
    pub fn is_branch(&self) -> bool {
        matches!(self, MptNode::Branch(_))
    }

    pub fn is_extension(&self) -> bool {
        matches!(self, MptNode::Extension(_))
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, MptNode::Leaf(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t1_1_branch_node_new() {
        let b = BranchNode::new();
        for c in &b.children {
            assert!(c.is_none());
        }
        assert!(b.value.is_none());
    }

    #[test]
    fn t1_2_child_ref_variants() {
        let arena = ChildRef::Arena(42);
        let hash = ChildRef::Hash(B256::ZERO);
        let inline = ChildRef::Inline(vec![0x80]);
        // Verify Debug output exists (no panic)
        let _ = format!("{arena:?}");
        let _ = format!("{hash:?}");
        let _ = format!("{inline:?}");
    }

    #[test]
    fn t1_3_branch_child_count() {
        let mut b = BranchNode::new();
        assert_eq!(b.child_count(), 0);
        b.children[0] = Some(ChildRef::Arena(0));
        assert_eq!(b.child_count(), 1);
        b.children[5] = Some(ChildRef::Arena(1));
        assert_eq!(b.child_count(), 2);
        b.children[15] = Some(ChildRef::Arena(2));
        assert_eq!(b.child_count(), 3);
    }

    #[test]
    fn t1_4_branch_single_child() {
        let mut b = BranchNode::new();
        // 0 children
        assert!(b.single_child().is_none());

        // 1 child
        b.children[7] = Some(ChildRef::Arena(42));
        let (idx, _) = b.single_child().unwrap();
        assert_eq!(idx, 7);

        // 2 children
        b.children[3] = Some(ChildRef::Arena(99));
        assert!(b.single_child().is_none());
    }

    #[test]
    fn t1_5_mpt_node_type_checks() {
        let branch = MptNode::Branch(BranchNode::new());
        assert!(branch.is_branch());
        assert!(!branch.is_extension());
        assert!(!branch.is_leaf());

        let ext = MptNode::Extension(ExtensionNode {
            nibbles: Nibbles::from_nibbles(&[1, 2]),
            child: ChildRef::Arena(0),
        });
        assert!(!ext.is_branch());
        assert!(ext.is_extension());
        assert!(!ext.is_leaf());

        let leaf =
            MptNode::Leaf(LeafNode { nibbles: Nibbles::from_nibbles(&[3, 4]), value: vec![0xaa] });
        assert!(!leaf.is_branch());
        assert!(!leaf.is_extension());
        assert!(leaf.is_leaf());
    }
}
