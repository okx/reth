use std::cmp::Ordering;

use seidb_common::error::{Result, SeiDbError};

/// Encode a key and version into MVCC format.
///
/// Format: `<key>\x00[<8-byte-BE-version>]<#version-bytes>`
///
/// If version > 0, appends the 8-byte big-endian version and a trailing length
/// byte of 0x09 (= 1 separator byte + 8 version bytes).
/// If version == 0, only appends the 0x00 separator (which doubles as the
/// zero-length indicator).
pub fn mvcc_encode(key: &[u8], version: i64) -> Vec<u8> {
    let mut dst = Vec::with_capacity(key.len() + 1 + if version > 0 { 9 } else { 0 });
    dst.extend_from_slice(key);
    dst.push(0x00);

    if version > 0 {
        let extra: u8 = 1 + 8;
        encode_uint64_ascending(&mut dst, version as u64);
        dst.push(extra);
    }

    dst
}

/// Split an MVCC-encoded key into the user key and optional version bytes.
///
/// Returns `None` if the key is empty or corrupt.
/// The returned slices borrow directly from `mvcc_key` (zero-copy).
pub fn split_mvcc_key(mvcc_key: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    if mvcc_key.is_empty() {
        return None;
    }

    let n = mvcc_key.len() - 1;
    let ts_len = mvcc_key[n] as usize;

    if n < ts_len {
        return None;
    }

    let user_key = &mvcc_key[..n - ts_len];

    let version_bytes = if ts_len > 0 {
        // Skip the 0x00 separator byte: [n - ts_len + 1 .. n)
        Some(&mvcc_key[n - ts_len + 1..n])
    } else {
        None
    };

    Some((user_key, version_bytes))
}

/// Append a u64 value as 8 big-endian bytes.
pub fn encode_uint64_ascending(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_be_bytes());
}

/// Decode a big-endian u64 from the first 8 bytes and return as i64.
///
/// Returns an error if fewer than 8 bytes are provided or the value exceeds
/// `i64::MAX`.
pub fn decode_uint64_ascending(b: &[u8]) -> Result<i64> {
    if b.len() < 8 {
        return Err(SeiDbError::Other(format!(
            "insufficient bytes to decode uint64 int value; expected 8; got {}",
            b.len()
        )));
    }

    let arr: [u8; 8] = b[..8]
        .try_into()
        .map_err(|_| SeiDbError::Other("invalid slice length for u64 decode".into()))?;
    let uv = u64::from_be_bytes(arr);
    if uv > i64::MAX as u64 {
        return Err(SeiDbError::Other(format!("uint64 value overflows int64: {uv}")));
    }

    Ok(uv as i64)
}

/// Compare two MVCC-encoded keys.
///
/// Keys are compared first by their user-key portion (lexicographic), then by
/// their timestamp portion. A key with no timestamp sorts before a key with a
/// timestamp when user keys are equal.
pub fn mvcc_key_compare(a: &[u8], b: &[u8]) -> Ordering {
    let a_end = a.len().wrapping_sub(1) as isize;
    let b_end = b.len().wrapping_sub(1) as isize;

    if a_end < 0 || b_end < 0 {
        return a.cmp(b);
    }

    let a_end = a_end as usize;
    let b_end = b_end as usize;

    let a_sep = a_end as isize - a[a_end] as isize;
    let b_sep = b_end as isize - b[b_end] as isize;

    if a_sep < 0 || b_sep < 0 {
        return a.cmp(b);
    }

    let a_sep = a_sep as usize;
    let b_sep = b_sep as usize;

    // Compare user keys
    let cmp = a[..a_sep].cmp(&b[..b_sep]);
    if cmp != Ordering::Equal {
        return cmp;
    }

    // Compare timestamp portions
    let a_ts = &a[a_sep..a_end];
    let b_ts = &b[b_sep..b_end];

    if a_ts.is_empty() {
        if b_ts.is_empty() {
            return Ordering::Equal;
        }
        return Ordering::Less;
    } else if b_ts.is_empty() {
        return Ordering::Greater;
    }

    a_ts.cmp(b_ts)
}

/// Encode a value with an optional tombstone version marker.
///
/// Uses the same encoding format as `mvcc_encode`.
pub fn mvcc_encode_value(value: &[u8], tombstone: i64) -> Vec<u8> {
    mvcc_encode(value, tombstone)
}

/// Split an MVCC-encoded value into the raw value and optional tombstone bytes.
///
/// Uses the same decoding logic as `split_mvcc_key`.
pub fn split_mvcc_value(encoded: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    split_mvcc_key(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mvcc_encode_with_version() {
        let encoded = mvcc_encode(b"key", 42);
        // key(3) + 0x00(1) + version(8) + extra(1) = 13
        assert_eq!(encoded.len(), 13);
        assert_eq!(&encoded[..3], b"key");
        assert_eq!(encoded[3], 0x00);
        // Version 42 in big-endian
        let version_bytes = &encoded[4..12];
        assert_eq!(version_bytes, &42u64.to_be_bytes());
        // Trailing length byte: 1 + 8 = 9
        assert_eq!(encoded[12], 9);
    }

    #[test]
    fn test_mvcc_encode_without_version() {
        let encoded = mvcc_encode(b"key", 0);
        // key(3) + 0x00(1) = 4
        assert_eq!(encoded.len(), 4);
        assert_eq!(&encoded[..3], b"key");
        assert_eq!(encoded[3], 0x00);
    }

    #[test]
    fn test_split_mvcc_key_with_version() {
        let encoded = mvcc_encode(b"hello", 100);
        let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
        assert_eq!(user_key, b"hello");
        let vb = version_bytes.unwrap();
        assert_eq!(vb.len(), 8);
        let version = decode_uint64_ascending(vb).unwrap();
        assert_eq!(version, 100);
    }

    #[test]
    fn test_split_mvcc_key_without_version() {
        let encoded = mvcc_encode(b"hello", 0);
        let (user_key, version_bytes) = split_mvcc_key(&encoded).unwrap();
        assert_eq!(user_key, b"hello");
        assert!(version_bytes.is_none());
    }

    #[test]
    fn test_split_mvcc_key_empty() {
        assert!(split_mvcc_key(&[]).is_none());
    }

    #[test]
    fn test_encode_decode_uint64() {
        for &v in &[0u64, 1, 42, 255, 65535, u32::MAX as u64, i64::MAX as u64] {
            let mut buf = Vec::new();
            encode_uint64_ascending(&mut buf, v);
            assert_eq!(buf.len(), 8);
            let decoded = decode_uint64_ascending(&buf).unwrap();
            assert_eq!(decoded, v as i64);
        }
    }

    #[test]
    fn test_decode_uint64_overflow() {
        let mut buf = Vec::new();
        encode_uint64_ascending(&mut buf, u64::MAX);
        assert!(decode_uint64_ascending(&buf).is_err());
    }

    #[test]
    fn test_mvcc_key_compare_different_keys() {
        let a = mvcc_encode(b"abc", 1);
        let b = mvcc_encode(b"abd", 1);
        assert_eq!(mvcc_key_compare(&a, &b), Ordering::Less);
        assert_eq!(mvcc_key_compare(&b, &a), Ordering::Greater);
    }

    #[test]
    fn test_mvcc_key_compare_same_key_different_version() {
        let a = mvcc_encode(b"key", 10);
        let b = mvcc_encode(b"key", 20);
        assert_eq!(mvcc_key_compare(&a, &b), Ordering::Less);
        assert_eq!(mvcc_key_compare(&b, &a), Ordering::Greater);
        assert_eq!(mvcc_key_compare(&a, &a), Ordering::Equal);
    }

    #[test]
    fn test_mvcc_key_compare_no_version() {
        let no_ver = mvcc_encode(b"key", 0);
        let with_ver = mvcc_encode(b"key", 5);
        // No-version key sorts before versioned key with the same user key
        assert_eq!(mvcc_key_compare(&no_ver, &with_ver), Ordering::Less);
        assert_eq!(mvcc_key_compare(&with_ver, &no_ver), Ordering::Greater);
    }

    #[test]
    fn test_mvcc_encode_value_with_tombstone() {
        let encoded = mvcc_encode_value(b"val", 5);
        assert_eq!(encoded.len(), 13); // val(3) + 0x00(1) + version(8) + extra(1)
        let (value, ts_bytes) = split_mvcc_value(&encoded).unwrap();
        assert_eq!(value, b"val");
        let ts = decode_uint64_ascending(ts_bytes.unwrap()).unwrap();
        assert_eq!(ts, 5);
    }

    #[test]
    fn test_mvcc_encode_value_no_tombstone() {
        let encoded = mvcc_encode_value(b"val", 0);
        assert_eq!(encoded.len(), 4); // val(3) + 0x00(1)
        let (value, ts_bytes) = split_mvcc_value(&encoded).unwrap();
        assert_eq!(value, b"val");
        assert!(ts_bytes.is_none());
    }
}
