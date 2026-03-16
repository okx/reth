use std::cmp::Ordering;

use crate::mvcc::encoding::mvcc_key_compare;

/// The comparator name used by the MVCC PebbleDB / RocksDB engine.
pub fn mvcc_comparator_name() -> &'static str {
    "ss_pebbledb_comparator"
}

/// Compare two MVCC-encoded keys using the custom MVCC ordering.
pub fn mvcc_compare_fn(a: &[u8], b: &[u8]) -> Ordering {
    mvcc_key_compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comparator_name() {
        assert_eq!(mvcc_comparator_name(), "ss_pebbledb_comparator");
    }
}
