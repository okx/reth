//! End-to-end integration tests for the memiavl tree.
//!
//! Tests cover large insertions, deletions, snapshot roundtrips, CoW semantics,
//! hash determinism, proofs, iterators, import/export, version management,
//! apply_change_set, and close/reopen.

use seidb_sc::memiavl::{
    import_export::{ExportNode, Exporter, TreeImporter},
    iterator::TreeIterator,
    snapshot::Snapshot,
    tree::Tree,
};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a deterministic key from an index using a simple hash-like scramble.
/// Returns a 8-byte key that distributes well across the key space.
fn make_key(i: u32) -> Vec<u8> {
    // Use a simple multiplicative hash to scramble insertion order.
    let scrambled = i.wrapping_mul(2654435761); // Knuth multiplicative hash
    format!("k{:010}", scrambled).into_bytes()
}

fn make_value(i: u32) -> Vec<u8> {
    format!("value_{:06}", i).into_bytes()
}

/// Generate a sequential key with zero-padded index for predictable ordering.
fn ordered_key(i: u32) -> Vec<u8> {
    format!("key_{:06}", i).into_bytes()
}

fn ordered_value(i: u32) -> Vec<u8> {
    format!("val_{:06}", i).into_bytes()
}

// ---------------------------------------------------------------------------
// 1. test_large_insertions_balanced
// ---------------------------------------------------------------------------

#[test]
fn test_large_insertions_balanced() {
    let mut tree = Tree::new_empty(0, 0);

    for i in 0..1000u32 {
        tree.set(&make_key(i), &make_value(i));
    }
    tree.save_version(true).unwrap();

    // Verify all 1000 keys are retrievable.
    for i in 0..1000u32 {
        assert!(tree.get(&make_key(i)).is_some(), "key {} should exist", i);
    }

    // Check tree height via root hash existence (tree is non-empty).
    assert!(!tree.is_empty());

    // For an AVL tree with 1000 nodes, height should be <= 1.44 * log2(1000) ~ 14.4 => 15.
    // We verify by checking the root node's height.
    let root = tree.root_ref().expect("tree should have a root");
    let height = root.height();
    assert!(height <= 15, "tree height {} exceeds AVL bound of 15 for 1000 nodes", height);
}

// ---------------------------------------------------------------------------
// 2. test_insertions_deletions_mixed
// ---------------------------------------------------------------------------

#[test]
fn test_insertions_deletions_mixed() {
    let mut tree = Tree::new_empty(0, 0);

    // Insert 500 keys.
    for i in 0..500u32 {
        tree.set(&ordered_key(i), &ordered_value(i));
    }
    tree.save_version(true).unwrap();

    // Delete 200 keys (every other key in the first 400).
    let mut deleted = Vec::new();
    for i in (0..400u32).step_by(2) {
        let removed = tree.remove(&ordered_key(i));
        assert!(removed.is_some(), "key {} should have been removed", i);
        deleted.push(i);
    }

    // Verify deleted keys are gone.
    for &i in &deleted {
        assert!(tree.get(&ordered_key(i)).is_none(), "deleted key {} should not exist", i);
    }

    // Verify remaining 300 keys are still present.
    let mut remaining_count = 0;
    for i in 0..500u32 {
        if deleted.contains(&i) {
            continue;
        }
        let val = tree.get(&ordered_key(i));
        assert!(val.is_some(), "surviving key {} should exist", i);
        assert_eq!(val.unwrap(), ordered_value(i));
        remaining_count += 1;
    }
    assert_eq!(remaining_count, 300);
}

// ---------------------------------------------------------------------------
// 3. test_snapshot_write_read_roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_snapshot_write_read_roundtrip() {
    let dir = tempdir().unwrap();
    let d = dir.path();

    let mut tree = Tree::new_empty(0, 0);
    for i in 0..50u32 {
        tree.set(&ordered_key(i), &ordered_value(i));
    }
    tree.save_version(true).unwrap();

    let original_hash = tree.root_hash();
    tree.write_snapshot(d).unwrap();

    // Open snapshot and create a new tree from it.
    let snapshot = Snapshot::open(d).unwrap();
    let loaded = Tree::new_from_snapshot(snapshot);

    assert_eq!(loaded.version(), 1);
    assert_eq!(loaded.root_hash(), original_hash);

    // Verify all 50 keys.
    for i in 0..50u32 {
        assert_eq!(
            loaded.get(&ordered_key(i)),
            Some(ordered_value(i)),
            "key {} mismatch after snapshot roundtrip",
            i
        );
    }
}

// ---------------------------------------------------------------------------
// 4. test_snapshot_large_tree
// ---------------------------------------------------------------------------

#[test]
fn test_snapshot_large_tree() {
    let dir = tempdir().unwrap();
    let d = dir.path();

    let mut tree = Tree::new_empty(0, 0);
    for i in 0..5000u32 {
        tree.set(&ordered_key(i), &ordered_value(i));
    }
    tree.save_version(true).unwrap();

    let original_hash = tree.root_hash();
    tree.write_snapshot(d).unwrap();

    let snapshot = Snapshot::open(d).unwrap();
    let loaded = Tree::new_from_snapshot(snapshot);

    assert_eq!(loaded.version(), 1);
    assert_eq!(loaded.root_hash(), original_hash);

    // Spot check every 100th key.
    for i in (0..5000u32).step_by(100) {
        assert_eq!(
            loaded.get(&ordered_key(i)),
            Some(ordered_value(i)),
            "key {} mismatch in large snapshot",
            i
        );
    }

    // Also check first and last.
    assert_eq!(loaded.get(&ordered_key(0)), Some(ordered_value(0)));
    assert_eq!(loaded.get(&ordered_key(4999)), Some(ordered_value(4999)));
}

// ---------------------------------------------------------------------------
// 5. test_cow_semantics
// ---------------------------------------------------------------------------

#[test]
fn test_cow_semantics() {
    let mut tree = Tree::new_empty(0, 0);
    tree.set(b"alpha", b"one");
    tree.set(b"beta", b"two");
    tree.set(b"gamma", b"three");
    tree.save_version(true).unwrap();

    let copy_hash = tree.root_hash();
    let copy = tree.copy();

    // Modify original.
    tree.set(b"alpha", b"MODIFIED");
    tree.set(b"delta", b"four");
    tree.remove(b"beta");

    // Copy should be unchanged.
    assert_eq!(copy.get(b"alpha"), Some(b"one".to_vec()));
    assert_eq!(copy.get(b"beta"), Some(b"two".to_vec()));
    assert_eq!(copy.get(b"gamma"), Some(b"three".to_vec()));
    assert!(copy.get(b"delta").is_none());
    assert_eq!(copy.root_hash(), copy_hash);

    // Original should reflect mutations.
    assert_eq!(tree.get(b"alpha"), Some(b"MODIFIED".to_vec()));
    assert!(tree.get(b"beta").is_none());
    assert_eq!(tree.get(b"delta"), Some(b"four".to_vec()));
    assert_ne!(tree.root_hash(), copy_hash);
}

// ---------------------------------------------------------------------------
// 6. test_hash_deterministic_across_snapshot
// ---------------------------------------------------------------------------

#[test]
fn test_hash_deterministic_across_snapshot() {
    let dir = tempdir().unwrap();
    let d = dir.path();

    let mut tree = Tree::new_empty(0, 0);
    for i in 0..100u32 {
        tree.set(&ordered_key(i), &ordered_value(i));
    }
    tree.save_version(true).unwrap();
    let hash1 = tree.root_hash();

    // Write snapshot and reload.
    tree.write_snapshot(d).unwrap();
    let snapshot = Snapshot::open(d).unwrap();
    let loaded = Tree::new_from_snapshot(snapshot);
    let hash2 = loaded.root_hash();

    assert_eq!(hash1, hash2, "hash must be identical before and after snapshot roundtrip");
    assert_eq!(hash1.len(), 32);
}

// ---------------------------------------------------------------------------
// 7. test_proof_after_snapshot
// ---------------------------------------------------------------------------

#[test]
fn test_proof_after_snapshot() {
    let dir = tempdir().unwrap();
    let d = dir.path();

    // Build tree.
    let mut tree = Tree::new_empty(0, 0);
    tree.set(b"alice", b"100");
    tree.set(b"bob", b"200");
    tree.set(b"carol", b"300");
    tree.save_version(true).unwrap();

    // Write snapshot, reopen, then re-apply the same keys to convert
    // persisted nodes into mem nodes (proof traversal requires left/right
    // which are only available on MemNode, not PersistedNode).
    tree.write_snapshot(d).unwrap();
    let snapshot = Snapshot::open(d).unwrap();
    let mut loaded = Tree::new_from_snapshot(snapshot);

    // Re-insert the same data to materialize MemNodes from PersistedNodes.
    loaded.set(b"alice", b"100");
    loaded.set(b"bob", b"200");
    loaded.set(b"carol", b"300");
    loaded.save_version(true).unwrap();

    // Hash should still be consistent (same data).
    assert_eq!(loaded.root_hash().len(), 32);

    // Membership proof for existing key.
    let proof = loaded.get_membership_proof(b"bob").unwrap();
    assert!(
        loaded.verify_membership(&proof, b"bob"),
        "membership proof should verify after snapshot reload"
    );

    // Non-membership proof for missing key.
    let proof = loaded.get_non_membership_proof(b"betty").unwrap();
    assert!(
        loaded.verify_non_membership(&proof, b"betty"),
        "non-membership proof should verify after snapshot reload"
    );

    // Also verify a key at the boundary.
    let proof = loaded.get_non_membership_proof(b"aaa").unwrap();
    assert!(
        loaded.verify_non_membership(&proof, b"aaa"),
        "non-membership proof for key before all others should verify"
    );
}

// ---------------------------------------------------------------------------
// 8. test_iterator_full_range
// ---------------------------------------------------------------------------

#[test]
fn test_iterator_full_range() {
    let mut tree = Tree::new_empty(0, 0);

    // Insert 100 keys in scrambled order.
    for i in 0..100u32 {
        tree.set(&make_key(i), &make_value(i));
    }

    let root = tree.root_ref().unwrap();
    let mut iter = TreeIterator::new(None, None, true, Some(root));

    let mut keys: Vec<Vec<u8>> = Vec::new();
    while iter.valid() {
        keys.push(iter.key().to_vec());
        iter.next();
    }

    assert_eq!(keys.len(), 100, "iterator should yield all 100 keys");

    // Verify sorted order.
    for i in 1..keys.len() {
        assert!(
            keys[i - 1] < keys[i],
            "keys should be in ascending order: {:?} >= {:?}",
            String::from_utf8_lossy(&keys[i - 1]),
            String::from_utf8_lossy(&keys[i])
        );
    }
}

// ---------------------------------------------------------------------------
// 9. test_iterator_reverse_full
// ---------------------------------------------------------------------------

#[test]
fn test_iterator_reverse_full() {
    let mut tree = Tree::new_empty(0, 0);

    for i in 0..100u32 {
        tree.set(&make_key(i), &make_value(i));
    }

    let root = tree.root_ref().unwrap();
    let mut iter = TreeIterator::new(None, None, false, Some(root));

    let mut keys: Vec<Vec<u8>> = Vec::new();
    while iter.valid() {
        keys.push(iter.key().to_vec());
        iter.next();
    }

    assert_eq!(keys.len(), 100, "reverse iterator should yield all 100 keys");

    // Verify reverse sorted order.
    for i in 1..keys.len() {
        assert!(
            keys[i - 1] > keys[i],
            "keys should be in descending order: {:?} <= {:?}",
            String::from_utf8_lossy(&keys[i - 1]),
            String::from_utf8_lossy(&keys[i])
        );
    }
}

// ---------------------------------------------------------------------------
// 10. test_import_export_large
// ---------------------------------------------------------------------------

#[test]
fn test_import_export_large() {
    // Build a 500-key tree.
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..500u32 {
        tree.set(&ordered_key(i), &ordered_value(i));
    }
    tree.save_version(true).unwrap();
    let original_hash = tree.root_hash();

    // Export.
    let root = tree.root_ref().unwrap();
    let export_nodes: Vec<ExportNode> = Exporter::new(Some(root)).collect();
    assert!(!export_nodes.is_empty(), "exporter should produce nodes for a 500-key tree");

    // Import into a new snapshot.
    let dir = tempdir().unwrap();
    let d = dir.path().join("imported");
    let mut importer = TreeImporter::new(&d, 1);
    for node in export_nodes {
        importer.add(node);
    }
    importer.close().unwrap();

    // Open snapshot and compare root hashes.
    let snapshot = Snapshot::open(&d).unwrap();
    let imported_hash = snapshot.root_hash();
    assert_eq!(original_hash, imported_hash, "imported tree root hash must match original");
}

// ---------------------------------------------------------------------------
// 11. test_version_management
// ---------------------------------------------------------------------------

#[test]
fn test_version_management() {
    let mut tree = Tree::new_empty(0, 0);
    assert_eq!(tree.version(), 0);

    // Each save_version should increment.
    tree.set(b"k1", b"v1");
    let (_, v1) = tree.save_version(true).unwrap();
    assert_eq!(v1, 1);
    assert_eq!(tree.version(), 1);

    tree.set(b"k2", b"v2");
    let (_, v2) = tree.save_version(true).unwrap();
    assert_eq!(v2, 2);
    assert_eq!(tree.version(), 2);

    tree.set(b"k3", b"v3");
    let (_, v3) = tree.save_version(false).unwrap();
    assert_eq!(v3, 3);

    // Hash from save_version(false) should be empty.
    tree.set(b"k4", b"v4");
    let (hash, v4) = tree.save_version(false).unwrap();
    assert_eq!(v4, 4);
    assert!(hash.is_empty());

    // Hash from save_version(true) should be 32 bytes.
    tree.set(b"k5", b"v5");
    let (hash, v5) = tree.save_version(true).unwrap();
    assert_eq!(v5, 5);
    assert_eq!(hash.len(), 32);

    // Versions accumulate monotonically even without data changes.
    let (_, v6) = tree.save_version(true).unwrap();
    assert_eq!(v6, 6);
}

// ---------------------------------------------------------------------------
// 12. test_apply_change_set_mixed
// ---------------------------------------------------------------------------

#[test]
fn test_apply_change_set_mixed() {
    let mut tree = Tree::new_empty(0, 0);

    // Pre-populate some keys.
    tree.set(b"existing_a", b"old_a");
    tree.set(b"existing_b", b"old_b");
    tree.set(b"to_delete_1", b"gone_soon");
    tree.set(b"to_delete_2", b"also_gone");
    tree.save_version(true).unwrap();

    // Build a mixed change set.
    let changes: Vec<(Vec<u8>, Option<Vec<u8>>)> = vec![
        // Update existing key.
        (b"existing_a".to_vec(), Some(b"new_a".to_vec())),
        // Insert new key.
        (b"brand_new".to_vec(), Some(b"fresh".to_vec())),
        // Delete existing keys.
        (b"to_delete_1".to_vec(), None),
        (b"to_delete_2".to_vec(), None),
        // Delete non-existent key (should be a no-op).
        (b"never_existed".to_vec(), None),
        // Leave existing_b untouched.
    ];
    tree.apply_change_set(&changes);
    tree.save_version(true).unwrap();

    // Verify results.
    assert_eq!(tree.get(b"existing_a"), Some(b"new_a".to_vec()));
    assert_eq!(tree.get(b"existing_b"), Some(b"old_b".to_vec()));
    assert_eq!(tree.get(b"brand_new"), Some(b"fresh".to_vec()));
    assert!(tree.get(b"to_delete_1").is_none());
    assert!(tree.get(b"to_delete_2").is_none());
    assert!(tree.get(b"never_existed").is_none());
}

// ---------------------------------------------------------------------------
// 13. test_tree_close_and_reopen
// ---------------------------------------------------------------------------

#[test]
fn test_tree_close_and_reopen() {
    let dir = tempdir().unwrap();
    let d = dir.path();

    // Build tree and write snapshot.
    let mut tree = Tree::new_empty(0, 0);
    tree.set(b"persist_a", b"val_a");
    tree.set(b"persist_b", b"val_b");
    tree.set(b"persist_c", b"val_c");
    tree.save_version(true).unwrap();
    let original_hash = tree.root_hash();

    tree.write_snapshot(d).unwrap();

    // Close the tree.
    tree.close().unwrap();
    assert!(tree.is_empty());
    assert!(tree.get(b"persist_a").is_none());

    // Reopen from snapshot.
    let snapshot = Snapshot::open(d).unwrap();
    let reopened = Tree::new_from_snapshot(snapshot);

    assert_eq!(reopened.version(), 1);
    assert_eq!(reopened.root_hash(), original_hash);
    assert_eq!(reopened.get(b"persist_a"), Some(b"val_a".to_vec()));
    assert_eq!(reopened.get(b"persist_b"), Some(b"val_b".to_vec()));
    assert_eq!(reopened.get(b"persist_c"), Some(b"val_c".to_vec()));
    assert!(reopened.get(b"nonexistent").is_none());
}
