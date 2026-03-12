use crate::memiavl::node::NodeRef;
use std::sync::Arc;

/// Stack-based DFS iterator over an IAVL tree.
///
/// Supports forward (ascending) and reverse (descending) iteration with
/// optional range filtering via `start` (inclusive) and `end` (exclusive).
///
/// This is a direct port of the Go `memiavl.Iterator`.
pub struct TreeIterator {
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    ascending: bool,
    valid: bool,
    /// Stack for DFS traversal. Nodes are `Arc<Node>` so cloning is cheap.
    stack: Vec<NodeRef>,
    /// Cached key of the current leaf.
    current_key: Vec<u8>,
    /// Cached value of the current leaf.
    current_value: Vec<u8>,
}

impl TreeIterator {
    /// Create a new iterator over the tree rooted at `root`.
    ///
    /// - `start`: inclusive lower bound (None = unbounded)
    /// - `end`: exclusive upper bound (None = unbounded)
    /// - `ascending`: true for forward iteration, false for reverse
    /// - `root`: the root node of the tree (None = empty tree)
    ///
    /// The iterator is positioned at the first matching leaf after construction.
    pub fn new(
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        ascending: bool,
        root: Option<&NodeRef>,
    ) -> Self {
        let mut iter = Self {
            start: start.map(|s| s.to_vec()),
            end: end.map(|e| e.to_vec()),
            ascending,
            valid: true,
            stack: Vec::new(),
            current_key: Vec::new(),
            current_value: Vec::new(),
        };

        if let Some(r) = root {
            iter.stack.push(Arc::clone(r));
        }

        // Advance to the first valid leaf (mirrors Go: `iter.Next()` in constructor).
        iter.next();
        iter
    }

    /// Returns the iteration domain: (start, end).
    pub fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        (self.start.as_deref(), self.end.as_deref())
    }

    /// Returns true if the iterator is positioned at a valid leaf.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// Returns the key of the current leaf. Only valid when `valid()` is true.
    pub fn key(&self) -> &[u8] {
        &self.current_key
    }

    /// Returns the value of the current leaf. Only valid when `valid()` is true.
    pub fn value(&self) -> &[u8] {
        &self.current_value
    }

    /// Advance the iterator to the next leaf in order.
    ///
    /// Uses stack-based DFS: pop a node, if it is a leaf in range yield it,
    /// otherwise push children with range pruning. For ascending order, push
    /// right then left so left is visited first. For descending, push left
    /// then right so right is visited first.
    pub fn next(&mut self) {
        while let Some(node) = self.stack.pop() {
            let key = node.key();
            let after_start = match &self.start {
                None => true,
                Some(s) => s.as_slice() < key,
            };
            let before_end = match &self.end {
                None => true,
                Some(e) => key < e.as_slice(),
            };

            if node.is_leaf() {
                let start_or_after = after_start ||
                    match &self.start {
                        None => true,
                        Some(s) => s.as_slice() == key,
                    };
                if start_or_after && before_end {
                    self.current_key = key.to_vec();
                    self.current_value = node.value().to_vec();
                    return;
                }
            } else if self.ascending {
                // Push right first, then left — so left is popped (visited) first.
                if before_end && let Some(right) = node.right() {
                    self.stack.push(Arc::clone(right));
                }
                if after_start && let Some(left) = node.left() {
                    self.stack.push(Arc::clone(left));
                }
            } else {
                // Descending: push left first, then right — so right is popped first.
                if after_start && let Some(left) = node.left() {
                    self.stack.push(Arc::clone(left));
                }
                if before_end && let Some(right) = node.right() {
                    self.stack.push(Arc::clone(right));
                }
            }
        }

        self.valid = false;
    }

    /// Invalidate the iterator and release the stack.
    pub fn close(&mut self) {
        self.valid = false;
        self.stack.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memiavl::node::{MemNode, Node};
    use std::sync::Arc;

    /// Build a branch with the correct IAVL convention: the branch key is the
    /// leftmost key of the right subtree.
    fn make_branch(left: NodeRef, right: NodeRef, version: u32) -> NodeRef {
        // Find leftmost leaf key of the right subtree.
        let mut cursor = Arc::clone(&right);
        while !cursor.is_leaf() {
            let l = cursor.left().expect("branch must have left child");
            cursor = Arc::clone(l);
        }
        let sep_key = cursor.key().to_vec();

        // Use new_branch_node then override the key to the correct separator.
        let mut mem = MemNode::new_branch_node(left, right, version);
        mem.key = sep_key;
        Arc::new(Node::Mem(mem))
    }

    /// Build a small balanced tree with 5 leaves: a, b, c, d, e.
    ///
    /// ```text
    ///          root (c)
    ///         /        \
    ///       (b)         (c)
    ///      /   \       /   \
    ///    a:1   b:2   c:3   (d)
    ///                     /   \
    ///                   d:4   e:5
    /// ```
    ///
    /// Branch keys follow IAVL convention: leftmost key of right subtree.
    fn build_test_tree() -> NodeRef {
        let a = Arc::new(Node::Mem(MemNode::new_leaf_node(b"a".to_vec(), b"1".to_vec(), 1)));
        let b = Arc::new(Node::Mem(MemNode::new_leaf_node(b"b".to_vec(), b"2".to_vec(), 1)));
        let c = Arc::new(Node::Mem(MemNode::new_leaf_node(b"c".to_vec(), b"3".to_vec(), 1)));
        let d = Arc::new(Node::Mem(MemNode::new_leaf_node(b"d".to_vec(), b"4".to_vec(), 1)));
        let e = Arc::new(Node::Mem(MemNode::new_leaf_node(b"e".to_vec(), b"5".to_vec(), 1)));

        // Build bottom-up with correct separator keys.
        let ab = make_branch(a, b, 1);
        let de = make_branch(d, e, 1);
        let cde = make_branch(c, de, 1);
        make_branch(ab, cde, 1)
    }

    #[test]
    fn test_iterator_forward() {
        let root = build_test_tree();
        let mut iter = TreeIterator::new(None, None, true, Some(&root));

        let mut keys = Vec::new();
        let mut values = Vec::new();
        while iter.valid() {
            keys.push(String::from_utf8(iter.key().to_vec()).unwrap());
            values.push(String::from_utf8(iter.value().to_vec()).unwrap());
            iter.next();
        }
        assert_eq!(keys, vec!["a", "b", "c", "d", "e"]);
        assert_eq!(values, vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn test_iterator_reverse() {
        let root = build_test_tree();
        let mut iter = TreeIterator::new(None, None, false, Some(&root));

        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(String::from_utf8(iter.key().to_vec()).unwrap());
            iter.next();
        }
        assert_eq!(keys, vec!["e", "d", "c", "b", "a"]);
    }

    #[test]
    fn test_iterator_range() {
        let root = build_test_tree();
        // start="c" (inclusive), end="f" (exclusive) → c, d, e
        let mut iter = TreeIterator::new(Some(b"c"), Some(b"f"), true, Some(&root));

        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(String::from_utf8(iter.key().to_vec()).unwrap());
            iter.next();
        }
        assert_eq!(keys, vec!["c", "d", "e"]);
    }

    #[test]
    fn test_iterator_range_reverse() {
        let root = build_test_tree();
        // start="b" (inclusive), end="e" (exclusive) → d, c, b  (descending)
        let mut iter = TreeIterator::new(Some(b"b"), Some(b"e"), false, Some(&root));

        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(String::from_utf8(iter.key().to_vec()).unwrap());
            iter.next();
        }
        assert_eq!(keys, vec!["d", "c", "b"]);
    }

    #[test]
    fn test_iterator_empty_tree() {
        let iter = TreeIterator::new(None, None, true, None);
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_single_key() {
        let leaf =
            Arc::new(Node::Mem(MemNode::new_leaf_node(b"only".to_vec(), b"one".to_vec(), 1)));
        let mut iter = TreeIterator::new(None, None, true, Some(&leaf));

        assert!(iter.valid());
        assert_eq!(iter.key(), b"only");
        assert_eq!(iter.value(), b"one");
        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_close() {
        let root = build_test_tree();
        let mut iter = TreeIterator::new(None, None, true, Some(&root));
        assert!(iter.valid());
        iter.close();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_start_beyond_all_keys() {
        let root = build_test_tree();
        let iter = TreeIterator::new(Some(b"z"), None, true, Some(&root));
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_end_before_all_keys() {
        let root = build_test_tree();
        let iter = TreeIterator::new(None, Some(b"a"), true, Some(&root));
        // end is exclusive, "a" < all keys means nothing matches
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_domain() {
        let root = build_test_tree();
        let iter = TreeIterator::new(Some(b"b"), Some(b"d"), true, Some(&root));
        let (start, end) = iter.domain();
        assert_eq!(start, Some(b"b".as_slice()));
        assert_eq!(end, Some(b"d".as_slice()));
    }

    #[test]
    fn test_iterator_exact_range() {
        let root = build_test_tree();
        // start="b", end="c" → only "b"
        let mut iter = TreeIterator::new(Some(b"b"), Some(b"c"), true, Some(&root));
        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        iter.next();
        assert!(!iter.valid());
    }
}
