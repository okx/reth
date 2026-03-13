use rayon::prelude::*;

use super::lthash::LtHash;

/// A key-value pair with the previous value for LtHash delta computation.
pub struct KvPairWithLastValue {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// Empty Vec means this is a new key (no previous value).
    pub last_value: Vec<u8>,
    pub delete: bool,
}

/// Applies KV changes to `prev` and returns the updated LtHash.
///
/// For each pair: MixOut(hash(serialize(key, last_value))) if last_value is non-empty,
/// then MixIn(hash(serialize(key, value))) if not a delete.
/// Chooses serial or parallel computation based on the number of pairs.
pub fn compute_lt_hash(prev: &LtHash, kv_pairs: &[KvPairWithLastValue]) -> LtHash {
    if kv_pairs.is_empty() {
        return prev.clone();
    }

    let delta = if kv_pairs.len() < 100 {
        compute_delta_serial(kv_pairs)
    } else {
        compute_delta_parallel(kv_pairs)
    };

    let mut result = prev.clone();
    result.mix_in(&delta);
    result
}

/// Computes the LtHash delta for a changeset serially.
/// Uses a reusable buffer to avoid per-pair heap allocation.
fn compute_delta_serial(kv_pairs: &[KvPairWithLastValue]) -> LtHash {
    let mut delta = LtHash::new();
    let mut buf = Vec::with_capacity(256);
    for pair in kv_pairs {
        if !pair.last_value.is_empty() &&
            LtHash::serialize_kv_into(&mut buf, &pair.key, &pair.last_value)
        {
            delta.mix_out(&LtHash::hash(&buf));
        }
        if !pair.delete && LtHash::serialize_kv_into(&mut buf, &pair.key, &pair.value) {
            delta.mix_in(&LtHash::hash(&buf));
        }
    }
    delta
}

/// Computes the LtHash delta for a changeset in parallel using rayon.
fn compute_delta_parallel(kv_pairs: &[KvPairWithLastValue]) -> LtHash {
    kv_pairs
        .par_iter()
        .map(|pair| {
            let mut delta = LtHash::new();
            if !pair.last_value.is_empty() &&
                let Some(buf) = LtHash::serialize_kv(&pair.key, &pair.last_value)
            {
                delta.mix_out(&LtHash::hash(&buf));
            }
            if !pair.delete &&
                let Some(buf) = LtHash::serialize_kv(&pair.key, &pair.value)
            {
                delta.mix_in(&LtHash::hash(&buf));
            }
            delta
        })
        .reduce(LtHash::new, |mut acc, d| {
            acc.mix_in(&d);
            acc
        })
}

/// Returns the default number of LtHash worker threads (rayon's thread pool size).
pub fn default_lt_hash_workers() -> usize {
    rayon::current_num_threads()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pair(key: &[u8], value: &[u8], last_value: &[u8], delete: bool) -> KvPairWithLastValue {
        KvPairWithLastValue {
            key: key.to_vec(),
            value: value.to_vec(),
            last_value: last_value.to_vec(),
            delete,
        }
    }

    #[test]
    fn test_compute_lt_hash_insert() {
        // New key: empty last_value, delete=false -> only MixIn
        let prev = LtHash::new();
        let pairs = vec![make_pair(b"key1", b"value1", b"", false)];
        let result = compute_lt_hash(&prev, &pairs);

        // Manually compute expected: MixIn(hash(serialize(key1, value1)))
        let mut expected = prev.clone();
        let buf = LtHash::serialize_kv(b"key1", b"value1").unwrap();
        expected.mix_in(&LtHash::hash(&buf));

        assert_eq!(result, expected);
        assert!(!result.is_zero());
    }

    #[test]
    fn test_compute_lt_hash_delete() {
        // Delete: non-empty last_value, delete=true -> only MixOut
        let prev = LtHash::hash(b"some initial state");
        let pairs = vec![make_pair(b"key1", b"", b"old_value", true)];
        let result = compute_lt_hash(&prev, &pairs);

        let mut expected = prev.clone();
        let buf = LtHash::serialize_kv(b"key1", b"old_value").unwrap();
        expected.mix_out(&LtHash::hash(&buf));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_compute_lt_hash_update() {
        // Update: non-empty last_value + new value -> MixOut old + MixIn new
        let prev = LtHash::hash(b"base state");
        let pairs = vec![make_pair(b"key1", b"new_val", b"old_val", false)];
        let result = compute_lt_hash(&prev, &pairs);

        let mut expected = prev.clone();
        let buf_old = LtHash::serialize_kv(b"key1", b"old_val").unwrap();
        expected.mix_out(&LtHash::hash(&buf_old));
        let buf_new = LtHash::serialize_kv(b"key1", b"new_val").unwrap();
        expected.mix_in(&LtHash::hash(&buf_new));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_compute_lt_hash_empty() {
        let prev = LtHash::hash(b"non-zero state");
        let result = compute_lt_hash(&prev, &[]);
        assert_eq!(result, prev);
    }

    #[test]
    fn test_compute_lt_hash_large_parallel() {
        // 500 pairs forces the parallel path (>= 100)
        let prev = LtHash::new();
        let pairs: Vec<KvPairWithLastValue> = (0..500)
            .map(|i| {
                make_pair(format!("key{i}").as_bytes(), format!("val{i}").as_bytes(), b"", false)
            })
            .collect();

        let result = compute_lt_hash(&prev, &pairs);

        // Verify against serial computation
        let serial = compute_delta_serial(&pairs);
        let mut expected = prev.clone();
        expected.mix_in(&serial);

        assert_eq!(result, expected);
        assert!(!result.is_zero());
    }

    #[test]
    fn test_parallel_equals_serial() {
        // Explicitly call both paths and compare
        let pairs: Vec<KvPairWithLastValue> = (0..200)
            .map(|i| {
                if i % 3 == 0 {
                    // Insert
                    make_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes(), b"", false)
                } else if i % 3 == 1 {
                    // Update
                    make_pair(
                        format!("k{i}").as_bytes(),
                        format!("new{i}").as_bytes(),
                        format!("old{i}").as_bytes(),
                        false,
                    )
                } else {
                    // Delete
                    make_pair(format!("k{i}").as_bytes(), b"", format!("prev{i}").as_bytes(), true)
                }
            })
            .collect();

        let serial = compute_delta_serial(&pairs);
        let parallel = compute_delta_parallel(&pairs);

        assert_eq!(serial, parallel);
    }

    #[test]
    fn test_compute_lt_hash_deterministic() {
        let prev = LtHash::hash(b"seed");
        let pairs = vec![
            make_pair(b"a", b"1", b"", false),
            make_pair(b"b", b"2", b"old2", false),
            make_pair(b"c", b"", b"old3", true),
        ];

        let r1 = compute_lt_hash(&prev, &pairs);
        let r2 = compute_lt_hash(&prev, &pairs);
        assert_eq!(r1, r2);
    }
}
