//! Data format compatibility tests for MVCC key encoding.
//!
//! These tests verify that the Rust MVCC encoding produces byte-identical output
//! to the Go `MVCCEncode` implementation. The Go-generated fixture file
//! `testdata/mvcc_encoding.json` is loaded and verified against Rust output.

use mptdb_engine::mvcc::encoding::{mvcc_encode, mvcc_key_compare, split_mvcc_key};
use std::cmp::Ordering;

// -------------------------------------------------------------------
// Round 3: Cross-language verification using Go-generated fixtures
// -------------------------------------------------------------------

#[test]
fn test_mvcc_go_fixture_roundtrip() {
    // Load Go-generated fixture: testdata/mvcc_encoding.json
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/mvcc_encoding.json");
    let data = std::fs::read_to_string(fixture_path)
        .expect("Failed to read mvcc_encoding.json — run Go fixture generator first");
    let vectors: Vec<serde_json::Value> = serde_json::from_str(&data).unwrap();

    for (i, v) in vectors.iter().enumerate() {
        let key_hex = v["key"].as_str().unwrap();
        let version = v["version"].as_i64().unwrap();
        let expected_hex = v["encoded"].as_str().unwrap();

        let key_bytes = hex::decode(key_hex).unwrap();
        let expected_bytes = hex::decode(expected_hex).unwrap();

        // Verify encoding matches Go output
        let encoded = mvcc_encode(&key_bytes, version);
        assert_eq!(
            encoded, expected_bytes,
            "vector {i}: MVCCEncode({key_hex:?}, {version}) mismatch"
        );

        // Verify split roundtrip
        let (user_key, version_bytes) =
            split_mvcc_key(&encoded).unwrap_or_else(|| panic!("vector {i}: split_mvcc_key failed"));
        assert_eq!(user_key, &key_bytes[..], "vector {i}: user_key mismatch after split");

        if version > 0 {
            let vb = version_bytes.expect("expected version bytes for version > 0");
            assert_eq!(vb.len(), 8, "vector {i}: version bytes should be 8 bytes");
            let decoded = u64::from_be_bytes(vb.try_into().unwrap());
            assert_eq!(decoded, version as u64, "vector {i}: version roundtrip failed");
        } else {
            // version == 0 means no version bytes in the encoding
            assert!(
                version_bytes.is_none() || version_bytes.unwrap().is_empty(),
                "vector {i}: version 0 should have no version bytes"
            );
        }
    }

    // Sanity check that we tested all expected vectors
    assert_eq!(vectors.len(), 8, "expected 8 vectors from Go fixture");
}

// -------------------------------------------------------------------
// Original inline tests (retained for fast CI without Go dependency)
// -------------------------------------------------------------------

#[test]
fn test_mvcc_key_encoding_compat() {
    // Go MVCCEncode("key", 42):
    // key bytes: [107, 101, 121]
    // separator: [0]
    // version 42 as uint64 big-endian: [0, 0, 0, 0, 0, 0, 0, 42]
    // length byte: 9 (1 + 8)
    // Total: [107, 101, 121, 0, 0, 0, 0, 0, 0, 0, 0, 42, 9]
    let encoded = mvcc_encode(b"key", 42);
    assert_eq!(encoded, vec![107, 101, 121, 0, 0, 0, 0, 0, 0, 0, 0, 42, 9]);

    // Verify split roundtrip
    let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
    assert_eq!(user_key, b"key");
    assert!(version_bytes.is_some());
}

#[test]
fn test_mvcc_encode_version_zero() {
    // Go MVCCEncode("key", 0): key + \x00 (no version bytes, no length byte beyond sentinel)
    let encoded = mvcc_encode(b"key", 0);
    assert_eq!(encoded, vec![107, 101, 121, 0]); // "key" + sentinel
}

#[test]
fn test_mvcc_key_compare_compat() {
    // Go behavior: same user key, different versions — higher version bytes sort later
    let a = mvcc_encode(b"foo", 1);
    let b = mvcc_encode(b"foo", 2);
    assert_eq!(mvcc_key_compare(&a, &b), Ordering::Less);

    // No-version key sorts before versioned key (same user key)
    let no_ver = mvcc_encode(b"foo", 0);
    let with_ver = mvcc_encode(b"foo", 1);
    assert_eq!(mvcc_key_compare(&no_ver, &with_ver), Ordering::Less);
}

#[test]
fn test_mvcc_encode_large_version() {
    // Verify encoding of large version number
    let encoded = mvcc_encode(b"k", i64::MAX);
    let (key, ver) = split_mvcc_key(&encoded).unwrap();
    assert_eq!(key, b"k");
    assert!(ver.is_some());
    assert_eq!(ver.unwrap().len(), 8);
}

#[test]
fn test_mvcc_encode_empty_key() {
    // Go MVCCEncode("", 5): separator + version + length byte
    let encoded = mvcc_encode(b"", 5);
    assert_eq!(encoded, vec![0, 0, 0, 0, 0, 0, 0, 0, 5, 9]);

    let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
    assert_eq!(user_key, b"");
    assert!(version_bytes.is_some());
    assert_eq!(version_bytes.unwrap(), &5u64.to_be_bytes());
}

#[test]
fn test_mvcc_encode_empty_key_version_zero() {
    // Go MVCCEncode("", 0): just the sentinel byte
    let encoded = mvcc_encode(b"", 0);
    assert_eq!(encoded, vec![0]);
}

#[test]
fn test_mvcc_encode_version_one() {
    // Go MVCCEncode("abc", 1):
    // "abc" = [97, 98, 99], separator [0], version 1 BE = [0,0,0,0,0,0,0,1], len byte = 9
    let encoded = mvcc_encode(b"abc", 1);
    assert_eq!(encoded, vec![97, 98, 99, 0, 0, 0, 0, 0, 0, 0, 0, 1, 9]);
}

#[test]
fn test_mvcc_encode_version_256() {
    // version 256 as u64 BE: [0, 0, 0, 0, 0, 0, 1, 0]
    let encoded = mvcc_encode(b"x", 256);
    let expected = vec![
        b'x', 0, // key + separator
        0, 0, 0, 0, 0, 0, 1, 0, // version 256 BE
        9, // length byte
    ];
    assert_eq!(encoded, expected);
}

#[test]
fn test_mvcc_key_compare_different_user_keys() {
    // Lexicographic ordering of user keys takes priority over version
    let a = mvcc_encode(b"aaa", 100);
    let b = mvcc_encode(b"bbb", 1);
    assert_eq!(mvcc_key_compare(&a, &b), Ordering::Less);
    assert_eq!(mvcc_key_compare(&b, &a), Ordering::Greater);
}

#[test]
fn test_mvcc_key_compare_equal() {
    let a = mvcc_encode(b"same", 42);
    let b = mvcc_encode(b"same", 42);
    assert_eq!(mvcc_key_compare(&a, &b), Ordering::Equal);
}

#[test]
fn test_mvcc_key_compare_both_no_version() {
    let a = mvcc_encode(b"key", 0);
    let b = mvcc_encode(b"key", 0);
    assert_eq!(mvcc_key_compare(&a, &b), Ordering::Equal);
}

#[test]
fn test_mvcc_split_roundtrip_various_versions() {
    // Verify split roundtrip for a range of versions
    for version in [1i64, 2, 100, 1000, 65535, 1_000_000, i64::MAX] {
        let encoded = mvcc_encode(b"test_key", version);
        let (user_key, ver_bytes) = split_mvcc_key(&encoded).unwrap();
        assert_eq!(user_key, b"test_key", "failed for version {version}");
        let vb = ver_bytes.unwrap();
        assert_eq!(vb.len(), 8, "version bytes length wrong for version {version}");
        let decoded = u64::from_be_bytes(vb.try_into().unwrap());
        assert_eq!(decoded, version as u64, "roundtrip failed for version {version}");
    }
}
