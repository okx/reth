//! Parallel Bloom Filter (ParaBloom) for fast conflict detection between transaction frames.
//!
//! Each frame maintains separate bloom filters for its read set and write set.
//! Conflict detection checks for read-write and write-write overlaps across frames,
//! returning a bitmask of conflicting frame IDs.

/// Maximum number of concurrent frames supported.
pub const MAX_FRAMES: usize = 64;

/// Bloom filter size in bits (2^11).
const BLOOM_BITS: usize = 2048;

/// Number of independent hash functions used per insertion/query.
const NUM_HASHES: usize = 5;

/// Number of u64 words needed to store BLOOM_BITS bits.
const BLOOM_WORDS: usize = BLOOM_BITS / 64;

/// Truncated 10-byte hash used as the key for bloom filter operations.
/// Will be unified with the crw_sets module's definition later.
pub type ShortHash = [u8; 10];

/// Fixed-size bloom filter backed by an array of u64 words.
#[derive(Clone, Debug)]
struct BloomFilter {
    bits: [u64; BLOOM_WORDS],
}

impl BloomFilter {
    fn new() -> Self {
        Self { bits: [0u64; BLOOM_WORDS] }
    }

    /// Derive bit positions from a `ShortHash` and set them in the filter.
    /// Uses 5 non-overlapping 2-byte windows to produce independent hash values.
    fn insert(&mut self, hash: &ShortHash) {
        for i in 0..NUM_HASHES {
            let pos = self.bit_position(hash, i);
            self.bits[pos / 64] |= 1u64 << (pos % 64);
        }
    }

    /// Returns true if all hash-derived bit positions are set (probabilistic membership).
    fn contains(&self, hash: &ShortHash) -> bool {
        for i in 0..NUM_HASHES {
            let pos = self.bit_position(hash, i);
            if self.bits[pos / 64] & (1u64 << (pos % 64)) == 0 {
                return false;
            }
        }
        true
    }

    fn clear(&mut self) {
        self.bits = [0u64; BLOOM_WORDS];
    }

    fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    /// Compute the i-th bit position from a ShortHash.
    /// Each hash function reads bytes [2*i .. 2*i+2] as a little-endian u16,
    /// then reduces modulo BLOOM_BITS.
    #[inline]
    fn bit_position(&self, hash: &ShortHash, i: usize) -> usize {
        let offset = i * 2;
        let val = u16::from_le_bytes([hash[offset], hash[offset + 1]]);
        (val as usize) % BLOOM_BITS
    }
}

/// Parallel bloom filter array for conflict detection across transaction frames.
///
/// Maintains per-frame read and write bloom filters. Conflict detection follows
/// the rule that read-read is safe, but any read-write or write-write overlap
/// between frames signals a dependency.
/// Maximum number of hashes per set before bloom false-positive rate degrades.
/// With 2048-bit bloom and 5 hash functions, ~200 insertions gives ~50% fill rate.
/// Beyond this, false positives increase rapidly, causing unnecessary frame separation.
pub const SET_MAX_SIZE: usize = 200;

#[derive(Debug)]
pub struct ParaBloom {
    /// Read-set bloom filter for each frame.
    read_blooms: Vec<BloomFilter>,
    /// Write-set bloom filter for each frame.
    write_blooms: Vec<BloomFilter>,
    /// Number of hashes inserted into each frame's read bloom.
    read_counts: Vec<usize>,
    /// Number of hashes inserted into each frame's write bloom.
    write_counts: Vec<usize>,
}

impl ParaBloom {
    /// Create a new `ParaBloom` with all frames cleared.
    pub fn new() -> Self {
        Self {
            read_blooms: vec![BloomFilter::new(); MAX_FRAMES],
            write_blooms: vec![BloomFilter::new(); MAX_FRAMES],
            read_counts: vec![0; MAX_FRAMES],
            write_counts: vec![0; MAX_FRAMES],
        }
    }

    /// Insert read and write hashes into the bloom filters for `frame_id`.
    pub fn add(&mut self, frame_id: usize, reads: &[ShortHash], writes: &[ShortHash]) {
        for hash in reads {
            self.read_blooms[frame_id].insert(hash);
        }
        for hash in writes {
            self.write_blooms[frame_id].insert(hash);
        }
        self.read_counts[frame_id] += reads.len();
        self.write_counts[frame_id] += writes.len();
    }

    /// Returns a bitmask where bit `i` is set if frame `i` conflicts with the given sets.
    ///
    /// Conflict rules:
    /// - new reads vs existing writes -> conflict
    /// - new writes vs existing reads -> conflict
    /// - new writes vs existing writes -> conflict
    /// - new reads vs existing reads -> no conflict
    pub fn get_dep_mask(&self, reads: &[ShortHash], writes: &[ShortHash]) -> u64 {
        let mut mask: u64 = 0;
        for i in 0..MAX_FRAMES {
            if self.frame_conflicts(i, reads, writes) {
                mask |= 1u64 << i;
            }
        }
        mask
    }

    /// Clear a specific frame's bloom filters.
    pub fn clear(&mut self, frame_id: usize) {
        self.read_blooms[frame_id].clear();
        self.write_blooms[frame_id].clear();
        self.read_counts[frame_id] = 0;
        self.write_counts[frame_id] = 0;
    }

    /// Check if a frame's bloom filter sets have exceeded the safe size.
    /// When exceeded, false-positive rate degrades and the frame should be flushed.
    pub fn is_oversized(&self, frame_id: usize) -> bool {
        self.read_counts[frame_id] > SET_MAX_SIZE || self.write_counts[frame_id] > SET_MAX_SIZE
    }

    /// Clear all frames.
    pub fn clear_all(&mut self) {
        for i in 0..MAX_FRAMES {
            self.read_blooms[i].clear();
            self.write_blooms[i].clear();
        }
    }

    /// Check whether the given read/write sets conflict with frame `i`.
    #[inline]
    fn frame_conflicts(&self, i: usize, reads: &[ShortHash], writes: &[ShortHash]) -> bool {
        let rb = &self.read_blooms[i];
        let wb = &self.write_blooms[i];

        // Skip empty frames early.
        if rb.is_empty() && wb.is_empty() {
            return false;
        }

        // New reads hitting existing writes -> conflict.
        for hash in reads {
            if wb.contains(hash) {
                return true;
            }
        }

        // New writes hitting existing reads or existing writes -> conflict.
        for hash in writes {
            if rb.contains(hash) || wb.contains(hash) {
                return true;
            }
        }

        false
    }
}

impl Default for ParaBloom {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(seed: u8) -> ShortHash {
        [seed; 10]
    }

    // Produce a hash that is very likely distinct from make_hash(seed)
    fn make_different_hash(seed: u8) -> ShortHash {
        [seed.wrapping_add(128); 10]
    }

    #[test]
    fn test_bloom_filter_basic() {
        let mut bf = BloomFilter::new();
        let h = make_hash(1);
        assert!(!bf.contains(&h));
        bf.insert(&h);
        assert!(bf.contains(&h));

        let other = make_different_hash(1);
        assert!(!bf.contains(&other));
    }

    #[test]
    fn test_bloom_filter_clear() {
        let mut bf = BloomFilter::new();
        let h = make_hash(42);
        bf.insert(&h);
        assert!(bf.contains(&h));
        assert!(!bf.is_empty());

        bf.clear();
        assert!(!bf.contains(&h));
        assert!(bf.is_empty());
    }

    #[test]
    fn test_para_bloom_no_conflict() {
        let mut pb = ParaBloom::new();
        let reads = vec![make_hash(1), make_hash(2)];
        // Frame 0 has only reads.
        pb.add(0, &reads, &[]);
        // Querying with reads only should not conflict (read-read is safe).
        let mask = pb.get_dep_mask(&reads, &[]);
        assert_eq!(mask, 0);
    }

    #[test]
    fn test_para_bloom_read_write_conflict() {
        let mut pb = ParaBloom::new();
        let hashes = vec![make_hash(10)];
        // Frame 0 has writes.
        pb.add(0, &[], &hashes);
        // Querying with reads to same hashes -> conflict with frame 0's writes.
        let mask = pb.get_dep_mask(&hashes, &[]);
        assert_eq!(mask, 1);
    }

    #[test]
    fn test_para_bloom_write_read_conflict() {
        let mut pb = ParaBloom::new();
        let hashes = vec![make_hash(20)];
        // Frame 0 has reads.
        pb.add(0, &hashes, &[]);
        // Querying with writes to same hashes -> conflict with frame 0's reads.
        let mask = pb.get_dep_mask(&[], &hashes);
        assert_eq!(mask, 1);
    }

    #[test]
    fn test_para_bloom_write_write_conflict() {
        let mut pb = ParaBloom::new();
        let hashes = vec![make_hash(30)];
        // Frame 0 has writes.
        pb.add(0, &[], &hashes);
        // Querying with writes to same hashes -> conflict with frame 0's writes.
        let mask = pb.get_dep_mask(&[], &hashes);
        assert_eq!(mask, 1);
    }

    #[test]
    fn test_para_bloom_multiple_frames() {
        let mut pb = ParaBloom::new();

        let h0 = vec![make_hash(1)];
        let h1 = vec![make_hash(2)];
        let h2 = vec![make_hash(3)];

        // Frame 0 writes h0, frame 1 writes h1, frame 2 writes h2.
        pb.add(0, &[], &h0);
        pb.add(1, &[], &h1);
        pb.add(2, &[], &h2);

        // Read h0 and h2 -> conflicts with frames 0 and 2.
        let mask = pb.get_dep_mask(&[make_hash(1), make_hash(3)], &[]);
        assert_eq!(mask & 0b001, 0b001, "frame 0 should conflict");
        assert_eq!(mask & 0b100, 0b100, "frame 2 should conflict");
        assert_eq!(mask & 0b010, 0, "frame 1 should not conflict");
    }

    #[test]
    fn test_para_bloom_clear_frame() {
        let mut pb = ParaBloom::new();
        let hashes = vec![make_hash(50)];
        pb.add(0, &[], &hashes);

        // Confirm conflict exists.
        assert_ne!(pb.get_dep_mask(&hashes, &[]), 0);

        // Clear frame 0 and verify no conflict.
        pb.clear(0);
        assert_eq!(pb.get_dep_mask(&hashes, &[]), 0);
    }
}
