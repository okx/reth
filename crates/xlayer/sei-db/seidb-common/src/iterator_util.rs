/// Compute iteration bounds with a prefix.
///
/// If `prefix` is empty, returns `(start, end)` unchanged.
/// Otherwise, returns `(prefix||start, prefix||end)` — or if `end` is `None`,
/// returns `(prefix||start, copy_incr(prefix))` to cover the full prefix range.
pub fn iterate_with_prefix(
    prefix: &[u8],
    start: &[u8],
    end: Option<&[u8]>,
) -> (Vec<u8>, Option<Vec<u8>>) {
    if prefix.is_empty() {
        return (start.to_vec(), end.map(|e| e.to_vec()));
    }

    let begin = clone_append(prefix, start);

    let finish = match end {
        None => copy_incr(prefix),
        Some(e) => Some(clone_append(prefix, e)),
    };

    (begin, finish)
}

/// Concatenate `front` and `tail` into a new `Vec<u8>`.
fn clone_append(front: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(front.len() + tail.len());
    res.extend_from_slice(front);
    res.extend_from_slice(tail);
    res
}

/// Increment the last byte of the byte slice to compute an exclusive upper bound.
/// Returns `None` if all bytes are 0xFF (overflow).
///
/// # Panics
///
/// Panics if `bz` is empty.
pub fn copy_incr(bz: &[u8]) -> Option<Vec<u8>> {
    assert!(!bz.is_empty(), "copyIncr expects non-zero bz length");

    let mut result = bz.to_vec();
    for i in (0..result.len()).rev() {
        if result[i] < 0xFF {
            result[i] += 1;
            return Some(result);
        }
        result[i] = 0x00;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iterate_with_prefix() {
        let (begin, end) = iterate_with_prefix(b"pre_", b"start", Some(b"end"));
        assert_eq!(begin, b"pre_start");
        assert_eq!(end, Some(b"pre_end".to_vec()));
    }

    #[test]
    fn test_iterate_with_prefix_empty_end() {
        let (begin, end) = iterate_with_prefix(b"pre_", b"start", None);
        assert_eq!(begin, b"pre_start");
        // "pre_" last byte is b'_' = 0x5F, incremented to 0x60 = b'`'
        assert_eq!(end, Some(b"pre`".to_vec()));
    }

    #[test]
    fn test_iterate_with_prefix_empty_prefix() {
        let (begin, end) = iterate_with_prefix(b"", b"start", Some(b"end"));
        assert_eq!(begin, b"start");
        assert_eq!(end, Some(b"end".to_vec()));
    }

    #[test]
    fn test_iterate_with_prefix_empty_prefix_none_end() {
        let (begin, end) = iterate_with_prefix(b"", b"start", None);
        assert_eq!(begin, b"start");
        assert_eq!(end, None);
    }

    #[test]
    fn test_copy_incr() {
        assert_eq!(copy_incr(b"\x00"), Some(vec![0x01]));
        assert_eq!(copy_incr(b"\x01\x02"), Some(vec![0x01, 0x03]));
        assert_eq!(copy_incr(b"abc"), Some(b"abd".to_vec()));
    }

    #[test]
    fn test_copy_incr_carry() {
        // Last byte is 0xFF, carry to previous byte
        assert_eq!(copy_incr(b"\x01\xFF"), Some(vec![0x02, 0x00]));
    }

    #[test]
    fn test_copy_incr_overflow() {
        assert_eq!(copy_incr(&[0xFF]), None);
        assert_eq!(copy_incr(&[0xFF, 0xFF]), None);
        assert_eq!(copy_incr(&[0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    #[should_panic(expected = "non-zero bz length")]
    fn test_copy_incr_empty() {
        copy_incr(b"");
    }
}
