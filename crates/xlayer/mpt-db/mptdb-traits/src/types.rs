/// Options controlling write behavior.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// When true, the write will be flushed to stable storage before returning.
    pub sync: bool,
}

/// Options for creating iterators over a key range.
#[derive(Debug, Clone, Default)]
pub struct IterOptions {
    /// Inclusive lower bound of the iteration range.
    pub lower_bound: Option<Vec<u8>>,
    /// Exclusive upper bound of the iteration range.
    pub upper_bound: Option<Vec<u8>>,
}

/// A snapshot node representing a key/value pair for import/export.
#[derive(Debug, Clone)]
pub struct SnapshotNode {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// A raw snapshot node that additionally carries a version number.
#[derive(Debug, Clone)]
pub struct RawSnapshotNode {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub version: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_options_default() {
        let opts = WriteOptions::default();
        assert!(!opts.sync);
    }

    #[test]
    fn test_snapshot_node_clone() {
        let node = SnapshotNode { key: vec![1, 2, 3], value: vec![4, 5, 6] };
        let cloned = node.clone();
        assert_eq!(node.key, cloned.key);
        assert_eq!(node.value, cloned.value);
    }
}
