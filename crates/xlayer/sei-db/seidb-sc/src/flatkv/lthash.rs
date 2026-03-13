use seidb_common::error::{Result, SeiDbError};

/// Number of u16 limbs in the LtHash vector.
pub const LT_HASH_SIZE: usize = 1024;
/// Byte size of a serialized LtHash (1024 * 2).
pub const LT_HASH_BYTES: usize = LT_HASH_SIZE * 2; // 2048

/// A 1024-element u16 vector supporting homomorphic updates via MixIn/MixOut.
#[derive(Clone, PartialEq, Eq)]
pub struct LtHash {
    pub(crate) limbs: [u16; LT_HASH_SIZE],
}

impl LtHash {
    /// Creates a zero-initialized LtHash.
    pub fn new() -> Self {
        Self { limbs: [0u16; LT_HASH_SIZE] }
    }

    /// Sets all limbs to zero.
    pub fn reset(&mut self) {
        self.limbs = [0u16; LT_HASH_SIZE];
    }

    /// Returns true if all limbs are zero.
    pub fn is_zero(&self) -> bool {
        self.limbs.iter().all(|&v| v == 0)
    }

    /// Element-wise wrapping addition (mod 2^16).
    pub fn mix_in(&mut self, other: &LtHash) {
        for i in 0..LT_HASH_SIZE {
            self.limbs[i] = self.limbs[i].wrapping_add(other.limbs[i]);
        }
    }

    /// Element-wise wrapping subtraction (mod 2^16).
    pub fn mix_out(&mut self, other: &LtHash) {
        for i in 0..LT_HASH_SIZE {
            self.limbs[i] = self.limbs[i].wrapping_sub(other.limbs[i]);
        }
    }

    /// Returns the Blake3-256 checksum of the serialized vector (32 bytes).
    pub fn checksum(&self) -> [u8; 32] {
        let data = self.marshal();
        *blake3::hash(&data).as_bytes()
    }

    /// Serializes to 2048 bytes (each u16 as 2 little-endian bytes).
    pub fn marshal(&self) -> Vec<u8> {
        bytemuck::cast_slice(&self.limbs).to_vec()
    }

    /// Writes the serialization into a pre-allocated buffer (must be >= 2048 bytes).
    pub fn marshal_to(&self, buf: &mut [u8]) {
        assert!(buf.len() >= LT_HASH_BYTES, "buffer too small");
        // Use bytemuck for zero-copy cast on little-endian platforms.
        let src: &[u8] = bytemuck::cast_slice(&self.limbs);
        buf[..LT_HASH_BYTES].copy_from_slice(src);
    }

    /// Deserializes 2048 bytes into an LtHash.
    pub fn unmarshal(data: &[u8]) -> Result<Self> {
        if data.len() != LT_HASH_BYTES {
            return Err(SeiDbError::Other(format!(
                "invalid LtHash size: got {}, want {}",
                data.len(),
                LT_HASH_BYTES
            )));
        }
        let mut limbs = [0u16; LT_HASH_SIZE];
        // Zero-copy cast on little-endian.
        let src: &[u16] = bytemuck::cast_slice(data);
        limbs.copy_from_slice(src);
        Ok(Self { limbs })
    }

    /// Creates an LtHash from arbitrary data using Blake3 XOF expanded to 2048 bytes.
    #[allow(dead_code)]
    pub(crate) fn hash(data: &[u8]) -> LtHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(data);
        let mut reader = hasher.finalize_xof();
        let mut buf = [0u8; LT_HASH_BYTES];
        reader.fill(&mut buf);
        let mut limbs = [0u16; LT_HASH_SIZE];
        // Zero-copy cast on little-endian.
        let src: &[u16] = bytemuck::cast_slice(&buf);
        limbs.copy_from_slice(src);
        LtHash { limbs }
    }

    /// Encodes a KV pair with length-prefixed fields.
    /// Format: keyLen[4 LE] || key || valueLen[4 LE] || value.
    /// Returns None if key or value is empty.
    #[allow(dead_code)]
    pub(crate) fn serialize_kv(key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        if key.is_empty() || value.is_empty() {
            return None;
        }
        let key_len = key.len();
        let value_len = value.len();
        let mut buf = vec![0u8; 4 + key_len + 4 + value_len];
        let mut off = 0;
        buf[off..off + 4].copy_from_slice(&(key_len as u32).to_le_bytes());
        off += 4;
        buf[off..off + key_len].copy_from_slice(key);
        off += key_len;
        buf[off..off + 4].copy_from_slice(&(value_len as u32).to_le_bytes());
        off += 4;
        buf[off..off + value_len].copy_from_slice(value);
        Some(buf)
    }

    /// Like [`serialize_kv`] but writes into a reusable buffer to avoid
    /// per-call heap allocation. Returns `true` if serialization succeeded.
    #[allow(dead_code)]
    pub(crate) fn serialize_kv_into(buf: &mut Vec<u8>, key: &[u8], value: &[u8]) -> bool {
        if key.is_empty() || value.is_empty() {
            return false;
        }
        let key_len = key.len();
        let value_len = value.len();
        let total = 4 + key_len + 4 + value_len;
        buf.clear();
        buf.reserve(total);
        buf.extend_from_slice(&(key_len as u32).to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(&(value_len as u32).to_le_bytes());
        buf.extend_from_slice(value);
        true
    }
}

impl Default for LtHash {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LtHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LtHash([{}, {}, {}, {}, ...])",
            self.limbs[0], self.limbs[1], self.limbs[2], self.limbs[3]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lthash_new_is_zero() {
        let h = LtHash::new();
        assert!(h.is_zero());
    }

    #[test]
    fn test_lthash_mix_in_out() {
        let mut a = LtHash::hash(b"hello");
        let original = a.clone();
        let b = LtHash::hash(b"world");
        a.mix_in(&b);
        assert_ne!(a, original);
        a.mix_out(&b);
        assert_eq!(a, original);
    }

    #[test]
    fn test_lthash_clone_equal() {
        let a = LtHash::hash(b"test data");
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn test_lthash_marshal_unmarshal() {
        let original = LtHash::hash(b"roundtrip");
        let data = original.marshal();
        assert_eq!(data.len(), LT_HASH_BYTES);
        let restored = LtHash::unmarshal(&data).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn test_lthash_checksum_deterministic() {
        let a = LtHash::hash(b"deterministic");
        let b = LtHash::hash(b"deterministic");
        assert_eq!(a.checksum(), b.checksum());
    }

    #[test]
    fn test_lthash_checksum_32_bytes() {
        let h = LtHash::hash(b"size check");
        let cs = h.checksum();
        assert_eq!(cs.len(), 32);
    }

    #[test]
    fn test_lthash_hash_consistency() {
        let a = LtHash::hash(b"same input");
        let b = LtHash::hash(b"same input");
        assert_eq!(a, b);
    }

    #[test]
    fn test_lthash_reset() {
        let mut h = LtHash::hash(b"non-zero");
        assert!(!h.is_zero());
        h.reset();
        assert!(h.is_zero());
    }

    #[test]
    fn test_lthash_serialize_kv_empty() {
        assert!(LtHash::serialize_kv(b"", b"value").is_none());
        assert!(LtHash::serialize_kv(b"key", b"").is_none());
        assert!(LtHash::serialize_kv(b"", b"").is_none());
    }

    #[test]
    fn test_lthash_serialize_kv_normal() {
        let result = LtHash::serialize_kv(b"key", b"value").unwrap();
        // keyLen(4) + key(3) + valueLen(4) + value(5) = 16
        assert_eq!(result.len(), 16);
        // Check key length prefix
        let key_len = u32::from_le_bytes([result[0], result[1], result[2], result[3]]);
        assert_eq!(key_len, 3);
        // Check key
        assert_eq!(&result[4..7], b"key");
        // Check value length prefix
        let val_len = u32::from_le_bytes([result[7], result[8], result[9], result[10]]);
        assert_eq!(val_len, 5);
        // Check value
        assert_eq!(&result[11..16], b"value");
    }

    #[test]
    fn test_lthash_homomorphic() {
        let a = LtHash::hash(b"alpha");
        let b = LtHash::hash(b"beta");

        // A + B
        let mut ab = a.clone();
        ab.mix_in(&b);

        // B + A
        let mut ba = b.clone();
        ba.mix_in(&a);

        assert_eq!(ab, ba, "MixIn should be commutative");
    }
}
