//! Tests for MVCC key encoding correctness.
//!
//! Verifies that `mvcc_encode`, `split_mvcc_key`, and `mvcc_key_compare`
//! produce the expected byte-level output for various key/version combinations.

use mptdb_engine::mvcc::encoding::{mvcc_encode, mvcc_key_compare, split_mvcc_key};
use std::cmp::Ordering;

#[test]
fn test_mvcc_key_encoding() {
    // "key" + separator + version(42, big-endian) + length byte
    let encoded = mvcc_encode(b"key", 42);
    assert_eq!(encoded, vec![107, 101, 121, 0, 0, 0, 0, 0, 0, 0, 0, 42, 9]);

    // Verify split roundtrip
    let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
    assert_eq!(user_key, b"key");
    assert!(version_bytes.is_some());
}

#[test]
fn test_mvcc_encode_version_zero() {
    // Version 0 encodes as key + sentinel only.
    let encoded = mvcc_encode(b"key", 0);
    assert_eq!(encoded, vec![107, 101, 121, 0]); // "key" + sentinel
}

#[test]
fn test_mvcc_key_compare() {
    // Same user key, different versions — higher version bytes sort later.
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
    // Empty user key still carries the separator, version, and length byte.
    let encoded = mvcc_encode(b"", 5);
    assert_eq!(encoded, vec![0, 0, 0, 0, 0, 0, 0, 0, 5, 9]);

    let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
    assert_eq!(user_key, b"");
    assert!(version_bytes.is_some());
    assert_eq!(version_bytes.unwrap(), &5u64.to_be_bytes());
}

#[test]
fn test_mvcc_encode_empty_key_version_zero() {
    // Empty user key at version 0 is just the sentinel byte.
    let encoded = mvcc_encode(b"", 0);
    assert_eq!(encoded, vec![0]);
}

#[test]
fn test_mvcc_encode_version_one() {
    // "abc" + separator + version 1 (big-endian) + length byte.
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
