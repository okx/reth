/// Increment version. If v == 0 and initial_version > 1, return initial_version.
/// Otherwise return v + 1.
pub fn next_version(v: i64, initial_version: u32) -> i64 {
    if v == 0 && initial_version > 1 {
        initial_version as i64
    } else {
        v + 1
    }
}

/// Convert version number to WAL/rlog index.
///
/// When `initial_version > 1`, index = version - initial_version + 1.
/// Otherwise, index = version (identity).
///
/// # Panics
///
/// Panics if `version` is negative.
pub fn version_to_index(version: i64, initial_version: u32) -> u64 {
    assert!(version >= 0, "version {} is out of range", version);
    if initial_version > 1 {
        (version as u64) - (initial_version as u64) + 1
    } else {
        version as u64
    }
}

/// Convert WAL/rlog index back to version number, reverse of `version_to_index`.
///
/// When `initial_version > 1`, version = index + initial_version - 1.
/// Otherwise, version = index (identity).
///
/// # Panics
///
/// Panics if `index > i64::MAX` or `initial_version > i32::MAX`.
pub fn index_to_version(index: u64, initial_version: u32) -> i64 {
    assert!(index <= i64::MAX as u64, "index {} is out of range", index);
    assert!(
        initial_version <= i32::MAX as u32,
        "initial version {} is out of range",
        initial_version
    );
    if initial_version > 1 {
        index as i64 + initial_version as i64 - 1
    } else {
        index as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_version() {
        // Normal increment
        assert_eq!(next_version(1, 0), 2);
        assert_eq!(next_version(5, 0), 6);

        // initial_version=0, v=0 => 0+1=1
        assert_eq!(next_version(0, 0), 1);

        // initial_version=1, v=0 => not >1, so 0+1=1
        assert_eq!(next_version(0, 1), 1);

        // initial_version=5, v=0 => returns 5
        assert_eq!(next_version(0, 5), 5);

        // initial_version=5, v=5 => 5+1=6 (only triggers when v==0)
        assert_eq!(next_version(5, 5), 6);
    }

    #[test]
    fn test_version_index_roundtrip() {
        // With initial_version=0, identity mapping
        for v in 0i64..10 {
            let idx = version_to_index(v, 0);
            assert_eq!(idx, v as u64);
            let back = index_to_version(idx, 0);
            assert_eq!(back, v);
        }
    }

    #[test]
    fn test_version_to_index_initial_version() {
        // initial_version=5: version_to_index(5, 5) = 5 - 5 + 1 = 1
        assert_eq!(version_to_index(5, 5), 1);
        assert_eq!(version_to_index(6, 5), 2);
        assert_eq!(version_to_index(10, 5), 6);

        // Roundtrip with initial_version=5
        // index 0 maps to version 4 (before initial), so start from index 1
        for idx in 1u64..10 {
            let v = index_to_version(idx, 5);
            let back = version_to_index(v, 5);
            assert_eq!(back, idx);
        }
    }

    #[test]
    #[should_panic(expected = "is out of range")]
    fn test_version_to_index_negative_panics() {
        version_to_index(-1, 0);
    }
}
