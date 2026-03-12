//! Data format compatibility tests for LtHash computation.
//!
//! These tests verify that the Rust LtHash implementation produces deterministic,
//! correct results matching the Go `lthash` package behavior. Go-generated golden
//! values from `testdata/lthash_golden.json` are used for cross-language verification.

use seidb_sc::flatkv::{
    lthash::LtHash,
    lthash_compute::{compute_lt_hash, KvPairWithLastValue},
};

fn make_pair(key: &[u8], value: &[u8], last_value: &[u8], delete: bool) -> KvPairWithLastValue {
    KvPairWithLastValue {
        key: key.to_vec(),
        value: value.to_vec(),
        last_value: last_value.to_vec(),
        delete,
    }
}

// -------------------------------------------------------------------
// Round 3: Cross-language verification using Go-generated fixtures
// -------------------------------------------------------------------

#[test]
fn test_lthash_go_fixture_golden_values() {
    // Load Go-generated fixture: testdata/lthash_golden.json
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/lthash_golden.json");
    let data = std::fs::read_to_string(fixture_path)
        .expect("Failed to read lthash_golden.json — run Go fixture generator first");
    let vectors: Vec<serde_json::Value> = serde_json::from_str(&data).unwrap();

    for (i, v) in vectors.iter().enumerate() {
        let description = v["description"].as_str().unwrap();
        let expected_checksum_hex = v["checksum"].as_str().unwrap();
        let expected_result_hex = v["result_hash"].as_str().unwrap();
        let prev_hex = v["prev_hash"].as_str().unwrap();
        let pairs_json = v["pairs"].as_array().unwrap();

        // Parse prev hash
        let prev = if prev_hex.is_empty() {
            LtHash::new()
        } else {
            let prev_bytes = hex::decode(prev_hex).unwrap();
            LtHash::unmarshal(&prev_bytes).unwrap()
        };

        // Parse KV pairs
        let pairs: Vec<KvPairWithLastValue> = pairs_json
            .iter()
            .map(|p| {
                let key = hex::decode(p["key"].as_str().unwrap()).unwrap();
                let value = hex::decode(p["value"].as_str().unwrap()).unwrap();
                let last_value = hex::decode(p["last_value"].as_str().unwrap()).unwrap();
                let delete = p["delete"].as_bool().unwrap();
                KvPairWithLastValue { key, value, last_value, delete }
            })
            .collect();

        // Compute result
        let result = compute_lt_hash(&prev, &pairs);

        // Verify marshaled result matches Go output
        let result_bytes = result.marshal();
        let expected_result_bytes = hex::decode(expected_result_hex).unwrap();
        assert_eq!(
            result_bytes, expected_result_bytes,
            "vector {i} ({description}): marshaled LtHash mismatch — Rust result differs from Go"
        );

        // Verify checksum matches Go output
        let checksum = result.checksum();
        let expected_checksum = hex::decode(expected_checksum_hex).unwrap();
        assert_eq!(
            checksum.as_slice(),
            expected_checksum.as_slice(),
            "vector {i} ({description}): checksum mismatch"
        );
    }

    assert_eq!(vectors.len(), 4, "expected 4 LtHash golden vectors");
}

#[test]
fn test_lthash_go_golden_single_insert() {
    // Hardcoded golden value from Go: single insert "account_key" -> "account_value"
    // Go checksum: aa246370c3958b0ad3134e751ee1f6e7b98aed2155f9881b77cbd94cff52786a
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"account_key", b"account_value", b"", false)];
    let result = compute_lt_hash(&prev, &pairs);
    let checksum = result.checksum();
    let expected =
        hex::decode("aa246370c3958b0ad3134e751ee1f6e7b98aed2155f9881b77cbd94cff52786a").unwrap();
    assert_eq!(checksum.as_slice(), expected.as_slice(), "single insert golden checksum mismatch");
}

#[test]
fn test_lthash_go_golden_two_inserts() {
    // Go checksum: 9f099db34f9bb64815ad9377bd62891fd035d5ee3498a7b99d4e055ea178ea08
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"k1", b"v1", b"", false), make_pair(b"k2", b"v2", b"", false)];
    let result = compute_lt_hash(&prev, &pairs);
    let checksum = result.checksum();
    let expected =
        hex::decode("9f099db34f9bb64815ad9377bd62891fd035d5ee3498a7b99d4e055ea178ea08").unwrap();
    assert_eq!(checksum.as_slice(), expected.as_slice(), "two inserts golden checksum mismatch");
}

#[test]
fn test_lthash_go_golden_insert_delete_zero() {
    // After insert then delete, Go checksum should match blake3(all-zeros-2048)
    // Go checksum: be2a8de3dcf46c94ce85cdc8e07ac308f4d8a95490d956c38d780fd610db0813
    let prev = LtHash::new();

    // Insert
    let pairs1 = vec![make_pair(b"key1", b"val1", b"", false)];
    let after_insert = compute_lt_hash(&prev, &pairs1);

    // Delete
    let pairs2 = vec![make_pair(b"key1", b"", b"val1", true)];
    let after_delete = compute_lt_hash(&after_insert, &pairs2);

    assert!(after_delete.is_zero(), "after insert+delete should be zero");

    let checksum = after_delete.checksum();
    let expected =
        hex::decode("be2a8de3dcf46c94ce85cdc8e07ac308f4d8a95490d956c38d780fd610db0813").unwrap();
    assert_eq!(checksum.as_slice(), expected.as_slice(), "insert+delete zero checksum mismatch");
}

#[test]
fn test_lthash_go_golden_update() {
    // Go checksum for update with last_value:
    // 09a49fc06cc5b649af819942538bb190b3d3acef991240a3828ac8a3e886ecd1
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"mykey", b"new_value", b"old_value", false)];
    let result = compute_lt_hash(&prev, &pairs);
    let checksum = result.checksum();
    let expected =
        hex::decode("09a49fc06cc5b649af819942538bb190b3d3acef991240a3828ac8a3e886ecd1").unwrap();
    assert_eq!(checksum.as_slice(), expected.as_slice(), "update golden checksum mismatch");
}

// -------------------------------------------------------------------
// Original property-based tests (retained for fast CI)
// -------------------------------------------------------------------

#[test]
fn test_lthash_deterministic() {
    // Same inputs must always produce same checksum
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"account_key", b"account_value", b"", false)];

    let result1 = compute_lt_hash(&prev, &pairs);
    let result2 = compute_lt_hash(&prev, &pairs);
    assert_eq!(result1.checksum(), result2.checksum());
    assert!(!result1.is_zero()); // non-trivial
}

#[test]
fn test_lthash_insert_then_delete_returns_to_zero() {
    let prev = LtHash::new();

    // Insert
    let pairs1 = vec![make_pair(b"key1", b"val1", b"", false)];
    let after_insert = compute_lt_hash(&prev, &pairs1);

    // Delete (MixOut the same key+value)
    let pairs2 = vec![make_pair(b"key1", b"", b"val1", true)];
    let after_delete = compute_lt_hash(&after_insert, &pairs2);

    // Should return to zero
    assert!(after_delete.is_zero());
}

#[test]
fn test_lthash_order_independence() {
    // Inserting A then B should give same result as B then A
    let prev = LtHash::new();

    let pairs_ab = vec![make_pair(b"a", b"va", b"", false), make_pair(b"b", b"vb", b"", false)];
    let pairs_ba = vec![make_pair(b"b", b"vb", b"", false), make_pair(b"a", b"va", b"", false)];

    let result_ab = compute_lt_hash(&prev, &pairs_ab);
    let result_ba = compute_lt_hash(&prev, &pairs_ba);
    assert_eq!(result_ab.checksum(), result_ba.checksum());
}

#[test]
fn test_lthash_update_not_equal_to_insert() {
    // Updating a key (with last_value) should produce a different hash than inserting it fresh
    let prev = LtHash::new();

    let insert_pairs = vec![make_pair(b"key", b"new_val", b"", false)];
    let update_pairs = vec![make_pair(b"key", b"new_val", b"old_val", false)];

    let after_insert = compute_lt_hash(&prev, &insert_pairs);
    let after_update = compute_lt_hash(&prev, &update_pairs);

    // These should differ because update also MixOut the old value
    assert_ne!(after_insert.checksum(), after_update.checksum());
}

#[test]
fn test_lthash_multiple_insert_delete_roundtrip() {
    // Insert multiple keys, then delete them all — should return to zero
    let prev = LtHash::new();

    let inserts = vec![
        make_pair(b"k1", b"v1", b"", false),
        make_pair(b"k2", b"v2", b"", false),
        make_pair(b"k3", b"v3", b"", false),
    ];
    let after_inserts = compute_lt_hash(&prev, &inserts);
    assert!(!after_inserts.is_zero());

    let deletes = vec![
        make_pair(b"k1", b"", b"v1", true),
        make_pair(b"k2", b"", b"v2", true),
        make_pair(b"k3", b"", b"v3", true),
    ];
    let after_deletes = compute_lt_hash(&after_inserts, &deletes);
    assert!(after_deletes.is_zero());
}

#[test]
fn test_lthash_marshal_unmarshal_after_compute() {
    let prev = LtHash::new();
    let pairs =
        vec![make_pair(b"alpha", b"one", b"", false), make_pair(b"beta", b"two", b"", false)];
    let result = compute_lt_hash(&prev, &pairs);

    // Marshal -> unmarshal roundtrip should preserve the hash
    let bytes = result.marshal();
    let restored = LtHash::unmarshal(&bytes).unwrap();
    assert_eq!(result.checksum(), restored.checksum());
}

#[test]
fn test_lthash_checksum_is_32_bytes() {
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"x", b"y", b"", false)];
    let result = compute_lt_hash(&prev, &pairs);
    assert_eq!(result.checksum().len(), 32);
}

#[test]
fn test_lthash_empty_pairs_returns_prev() {
    let prev = LtHash::new();
    let pairs = vec![make_pair(b"data", b"value", b"", false)];
    let non_zero = compute_lt_hash(&prev, &pairs);

    // Empty changeset should return prev unchanged
    let same = compute_lt_hash(&non_zero, &[]);
    assert_eq!(non_zero.checksum(), same.checksum());
}
