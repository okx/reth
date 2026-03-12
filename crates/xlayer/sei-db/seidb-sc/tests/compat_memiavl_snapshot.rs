//! Data format compatibility tests for MemIAVL snapshot format.
//!
//! These tests verify snapshot format consistency (metadata magic bytes, node/leaf
//! record sizes, write-then-read roundtrips). Go-generated snapshot fixtures
//! in `testdata/memiavl_snapshot/` are loaded for cross-language verification.

use seidb_sc::memiavl::{
    layout::{LEAF_SIZE, NODE_SIZE},
    snapshot::Snapshot,
    tree::Tree,
};
use std::path::Path;
use tempfile::tempdir;

// -------------------------------------------------------------------
// Round 3: Cross-language verification using Go-generated snapshot
// -------------------------------------------------------------------

#[test]
fn test_read_go_generated_snapshot() {
    // Load Go-generated snapshot from testdata/memiavl_snapshot/
    let snapshot_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/memiavl_snapshot");
    let snap = Snapshot::open(Path::new(snapshot_dir))
        .expect("Failed to open Go-generated snapshot — run Go fixture generator first");

    // Verify metadata from Go: version=1,
    // root_hash=7e880ed352ceb970c1ac14c29217911b56972ae0c5a867bb5c209ebe0b32f950
    assert_eq!(snap.version(), 1);
    assert!(!snap.is_empty());

    // Verify root hash matches Go output
    let root_hash = snap.root_hash();
    let expected_root_hash =
        hex::decode("7e880ed352ceb970c1ac14c29217911b56972ae0c5a867bb5c209ebe0b32f950").unwrap();
    assert_eq!(
        root_hash, expected_root_hash,
        "root hash mismatch between Go-generated snapshot and Rust reader"
    );

    // Verify we can read the expected number of leaves (10 keys: 'a'..'j')
    assert_eq!(snap.leaf_count(), 10);

    // Verify key-value pairs match the Go fixture metadata
    let expected_keys: Vec<Vec<u8>> = (b'a'..=b'j').map(|b| vec![b]).collect();
    let expected_values: Vec<Vec<u8>> = (b'A'..=b'J').map(|b| vec![b]).collect();

    for i in 0..10 {
        let (k, v) = snap.leaf_key_value(i);
        assert_eq!(k, expected_keys[i as usize], "key mismatch at leaf {i}");
        assert_eq!(v, expected_values[i as usize], "value mismatch at leaf {i}");
    }
}

#[test]
fn test_go_snapshot_metadata_bytes() {
    // Verify the raw metadata file bytes match Go's format exactly
    let metadata_path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/memiavl_snapshot/metadata");
    let metadata = std::fs::read(metadata_path).expect("Failed to read Go-generated metadata file");

    assert_eq!(metadata.len(), 12); // magic(4) + format(4) + version(4)

    let magic = u32::from_le_bytes(metadata[0..4].try_into().unwrap());
    assert_eq!(magic, 0x4C564149, "magic mismatch"); // "IAVL" in LE

    let format = u32::from_le_bytes(metadata[4..8].try_into().unwrap());
    assert_eq!(format, 0, "format mismatch");

    let version = u32::from_le_bytes(metadata[8..12].try_into().unwrap());
    assert_eq!(version, 1, "version mismatch");
}

#[test]
fn test_go_snapshot_file_sizes() {
    // Verify Go-generated snapshot files have expected sizes
    let base = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/memiavl_snapshot");

    let nodes_data = std::fs::read(format!("{base}/nodes")).unwrap();
    let leaves_data = std::fs::read(format!("{base}/leaves")).unwrap();

    // nodes and leaves must be exact multiples of their record sizes
    assert_eq!(nodes_data.len() % NODE_SIZE, 0, "nodes file not multiple of NODE_SIZE");
    assert_eq!(leaves_data.len() % LEAF_SIZE, 0, "leaves file not multiple of LEAF_SIZE");

    // For 10 keys: 10 leaves, 9 branch nodes
    let branch_count = nodes_data.len() / NODE_SIZE;
    let leaf_count = leaves_data.len() / LEAF_SIZE;
    assert_eq!(leaf_count, 10, "expected 10 leaves");
    assert_eq!(branch_count + 1, leaf_count, "branches + 1 should equal leaves");
}

#[test]
fn test_rust_snapshot_matches_go_root_hash() {
    // Build the same tree in Rust that Go built (keys a..j, values A..J)
    // and verify the root hash is identical
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..10u8 {
        tree.set(&[b'a' + i], &[b'A' + i]);
    }
    tree.save_version(true).unwrap();

    let rust_hash = tree.root_hash();
    let go_hash =
        hex::decode("7e880ed352ceb970c1ac14c29217911b56972ae0c5a867bb5c209ebe0b32f950").unwrap();

    assert_eq!(
        rust_hash, go_hash,
        "Rust-built tree root hash differs from Go-generated snapshot root hash"
    );
}

#[test]
fn test_rust_snapshot_roundtrip_matches_go() {
    // Build tree in Rust, write snapshot, reopen, verify hash matches Go
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..10u8 {
        tree.set(&[b'a' + i], &[b'A' + i]);
    }
    tree.save_version(true).unwrap();

    let dir = tempdir().unwrap();
    tree.write_snapshot(dir.path()).unwrap();

    let snap = Snapshot::open(dir.path()).unwrap();
    let go_hash =
        hex::decode("7e880ed352ceb970c1ac14c29217911b56972ae0c5a867bb5c209ebe0b32f950").unwrap();
    assert_eq!(snap.root_hash(), go_hash);
    assert_eq!(snap.leaf_count(), 10);
}

// -------------------------------------------------------------------
// Original inline tests (retained for fast CI without Go dependency)
// -------------------------------------------------------------------

#[test]
fn test_snapshot_format_consistency() {
    // Build a deterministic tree and verify snapshot roundtrip
    let mut tree = Tree::new_empty(0, 0);
    tree.set(b"key_a", b"value_a");
    tree.set(b"key_b", b"value_b");
    tree.set(b"key_c", b"value_c");
    tree.save_version(true).unwrap();

    let hash_before = tree.root_hash();

    // Write snapshot
    let dir = tempdir().unwrap();
    tree.write_snapshot(dir.path()).unwrap();

    // Verify metadata file
    let metadata = std::fs::read(dir.path().join("metadata")).unwrap();
    assert_eq!(metadata.len(), 12); // magic(4) + format(4) + version(4)
    let magic = u32::from_le_bytes(metadata[0..4].try_into().unwrap());
    assert_eq!(magic, 0x4C564149); // "IAVL" in LE
    let format = u32::from_le_bytes(metadata[4..8].try_into().unwrap());
    assert_eq!(format, 0);
    let version = u32::from_le_bytes(metadata[8..12].try_into().unwrap());
    assert_eq!(version, 1);

    // Reopen and verify hash matches
    let snap = Snapshot::open(dir.path()).unwrap();
    let hash_after = snap.root_hash();
    assert_eq!(hash_before, hash_after);
}

#[test]
fn test_snapshot_node_leaf_sizes() {
    // Verify node and leaf record sizes match Go's constants
    assert_eq!(NODE_SIZE, 48);
    assert_eq!(LEAF_SIZE, 48);
}

#[test]
fn test_snapshot_empty_tree() {
    // An empty tree should produce valid but empty snapshot files
    let mut tree = Tree::new_empty(0, 0);
    tree.save_version(true).unwrap();

    let dir = tempdir().unwrap();
    tree.write_snapshot(dir.path()).unwrap();

    let metadata = std::fs::read(dir.path().join("metadata")).unwrap();
    assert_eq!(metadata.len(), 12);
    let magic = u32::from_le_bytes(metadata[0..4].try_into().unwrap());
    assert_eq!(magic, 0x4C564149);

    let nodes_data = std::fs::read(dir.path().join("nodes")).unwrap();
    let leaves_data = std::fs::read(dir.path().join("leaves")).unwrap();
    assert!(nodes_data.is_empty());
    assert!(leaves_data.is_empty());

    let snap = Snapshot::open(dir.path()).unwrap();
    assert!(snap.is_empty());
    assert_eq!(snap.version(), 1);
}

#[test]
fn test_snapshot_single_key() {
    // A single key tree should have 0 branch nodes and 1 leaf
    let mut tree = Tree::new_empty(0, 0);
    tree.set(b"only_key", b"only_value");
    tree.save_version(true).unwrap();

    let dir = tempdir().unwrap();
    tree.write_snapshot(dir.path()).unwrap();

    let snap = Snapshot::open(dir.path()).unwrap();
    assert_eq!(snap.node_count(), 0); // no branches
    assert_eq!(snap.leaf_count(), 1); // one leaf

    // Verify key-value roundtrip
    let (k, v) = snap.leaf_key_value(0);
    assert_eq!(k, b"only_key");
    assert_eq!(v, b"only_value");
}

#[test]
fn test_snapshot_nodes_file_size_multiple_of_node_size() {
    // The nodes file must be an exact multiple of NODE_SIZE
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..10u32 {
        tree.set(format!("key_{i:04}").as_bytes(), format!("val_{i:04}").as_bytes());
    }
    tree.save_version(true).unwrap();

    let dir = tempdir().unwrap();
    tree.write_snapshot(dir.path()).unwrap();

    let nodes_data = std::fs::read(dir.path().join("nodes")).unwrap();
    let leaves_data = std::fs::read(dir.path().join("leaves")).unwrap();
    assert_eq!(nodes_data.len() % NODE_SIZE, 0);
    assert_eq!(leaves_data.len() % LEAF_SIZE, 0);

    // branches + 1 == leaves for a non-empty tree
    let branch_count = nodes_data.len() / NODE_SIZE;
    let leaf_count = leaves_data.len() / LEAF_SIZE;
    assert_eq!(branch_count + 1, leaf_count);
}

#[test]
fn test_snapshot_deterministic_hash() {
    // Two trees built with the same data in the same order must produce identical hashes
    let build_tree = || {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"alpha", b"one");
        tree.set(b"beta", b"two");
        tree.set(b"gamma", b"three");
        tree.save_version(true).unwrap();
        tree.root_hash()
    };

    let hash1 = build_tree();
    let hash2 = build_tree();
    assert_eq!(hash1, hash2);
    assert!(!hash1.is_empty());
}

#[test]
fn test_snapshot_metadata_magic_constant() {
    // The magic constant must be exactly "IAVL" encoded as little-endian u32
    let expected = u32::from_le_bytes([b'I', b'A', b'V', b'L']);
    assert_eq!(expected, 0x4C564149);
    assert_eq!(seidb_sc::memiavl::snapshot::SNAPSHOT_MAGIC, expected);
}
