//! Commonly used types for static file usage.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod compression;
mod event;
mod segment;

use alloy_primitives::BlockNumber;
pub use compression::Compression;
use core::ops::RangeInclusive;
pub use event::StaticFileProducerEvent;
pub use segment::{SegmentConfig, SegmentHeader, SegmentRangeInclusive, StaticFileSegment};

/// Map keyed by [`StaticFileSegment`].
pub type StaticFileMap<T> = alloc::boxed::Box<fixed_map::Map<StaticFileSegment, T>>;

/// Default static file block count.
pub const DEFAULT_BLOCKS_PER_STATIC_FILE: u64 = 500_000;

/// Highest static file block numbers, per data segment.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct HighestStaticFiles {
    /// Highest static file block of receipts, inclusive.
    /// If [`None`], no static file is available.
    pub receipts: Option<BlockNumber>,
}

impl HighestStaticFiles {
    /// Returns an iterator over all static file segments
    fn iter(&self) -> impl Iterator<Item = Option<BlockNumber>> {
        [self.receipts].into_iter()
    }

    /// Returns the minimum block of all segments.
    pub fn min_block_num(&self) -> Option<u64> {
        self.iter().flatten().min()
    }

    /// Returns the maximum block of all segments.
    pub fn max_block_num(&self) -> Option<u64> {
        self.iter().flatten().max()
    }
}

/// Static File targets, per data segment, measured in [`BlockNumber`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StaticFileTargets {
    /// Targeted range of receipts.
    pub receipts: Option<RangeInclusive<BlockNumber>>,
}

impl StaticFileTargets {
    /// Returns `true` if any of the targets are [Some].
    pub const fn any(&self) -> bool {
        self.receipts.is_some()
    }

    /// Returns `true` if all targets are either [`None`] or has beginning of the range equal to the
    /// highest static file.
    pub fn is_contiguous_to_highest_static_files(&self, static_files: HighestStaticFiles) -> bool {
        core::iter::once(&(self.receipts.as_ref(), static_files.receipts)).all(
            |(target_block_range, highest_static_file_block)| {
                target_block_range.is_none_or(|target_block_range| {
                    *target_block_range.start() ==
                        highest_static_file_block
                            .map_or(0, |highest_static_file_block| highest_static_file_block + 1)
                })
            },
        )
    }
}

/// Each static file has a fixed number of blocks. This gives out the range where the requested
/// block is positioned, according to the specified number of blocks per static file.
pub const fn find_fixed_range(
    block: BlockNumber,
    blocks_per_static_file: u64,
) -> SegmentRangeInclusive {
    let start = (block / blocks_per_static_file) * blocks_per_static_file;
    SegmentRangeInclusive::new(start, start + blocks_per_static_file - 1)
}

/// Each static file has a fixed number of blocks. This gives out the range where the requested
/// block is positioned, accounting for a custom genesis block number.
///
/// For chains with custom genesis block numbers (e.g., genesis at block 8593921), this ensures
/// the first static file range starts at the genesis block, not at a multiple of
/// `blocks_per_static_file`.
///
/// # Arguments
///
/// * `block` - The block number to find the range for
/// * `blocks_per_static_file` - Number of blocks per static file
/// * `genesis_block_number` - The genesis block number of the chain
///
/// # Returns
///
/// The static file range that should contain the given block.
pub const fn find_fixed_range_with_genesis(
    block: BlockNumber,
    blocks_per_static_file: u64,
    genesis_block_number: BlockNumber,
) -> SegmentRangeInclusive {
    // For blocks before genesis, return a range starting from 0 (shouldn't happen in practice)
    if block < genesis_block_number {
        let start = (block / blocks_per_static_file) * blocks_per_static_file;
        return SegmentRangeInclusive::new(start, start + blocks_per_static_file - 1);
    }

    // Calculate how many blocks after genesis
    let blocks_since_genesis = block - genesis_block_number;
    
    // Calculate which segment this block belongs to
    let segment_index = blocks_since_genesis / blocks_per_static_file;
    
    // Calculate the start of this segment relative to genesis
    let start = genesis_block_number + (segment_index * blocks_per_static_file);
    
    SegmentRangeInclusive::new(start, start + blocks_per_static_file - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highest_static_files_min() {
        let files = HighestStaticFiles { receipts: Some(100) };

        // Minimum value among the available segments
        assert_eq!(files.min_block_num(), Some(100));

        let empty_files = HighestStaticFiles::default();
        // No values, should return None
        assert_eq!(empty_files.min_block_num(), None);
    }

    #[test]
    fn test_highest_static_files_max() {
        let files = HighestStaticFiles { receipts: Some(100) };

        // Maximum value among the available segments
        assert_eq!(files.max_block_num(), Some(100));

        let empty_files = HighestStaticFiles::default();
        // No values, should return None
        assert_eq!(empty_files.max_block_num(), None);
    }

    #[test]
    fn test_find_fixed_range() {
        // Test with default block size
        let block: BlockNumber = 600_000;
        let range = find_fixed_range(block, DEFAULT_BLOCKS_PER_STATIC_FILE);
        assert_eq!(range.start(), 500_000);
        assert_eq!(range.end(), 999_999);

        // Test with a custom block size
        let block: BlockNumber = 1_200_000;
        let range = find_fixed_range(block, 1_000_000);
        assert_eq!(range.start(), 1_000_000);
        assert_eq!(range.end(), 1_999_999);
    }

    #[test]
    fn test_find_fixed_range_with_genesis() {
        // Test with genesis at 0 (should behave like find_fixed_range)
        let block: BlockNumber = 600_000;
        let range = find_fixed_range_with_genesis(block, DEFAULT_BLOCKS_PER_STATIC_FILE, 0);
        assert_eq!(range.start(), 500_000);
        assert_eq!(range.end(), 999_999);

        // Test with custom genesis block number (the reported issue case)
        let genesis = 8593921;
        let blocks_per_file = 500_000;
        
        // Genesis block should be in range starting at genesis
        let range = find_fixed_range_with_genesis(genesis, blocks_per_file, genesis);
        assert_eq!(range.start(), 8593921);
        assert_eq!(range.end(), 9093920); // 8593921 + 500000 - 1

        // Next block after genesis should be in same range
        let range = find_fixed_range_with_genesis(genesis + 1, blocks_per_file, genesis);
        assert_eq!(range.start(), 8593921);
        assert_eq!(range.end(), 9093920);

        // Block at end of first segment
        let range = find_fixed_range_with_genesis(9093920, blocks_per_file, genesis);
        assert_eq!(range.start(), 8593921);
        assert_eq!(range.end(), 9093920);

        // First block of second segment
        let range = find_fixed_range_with_genesis(9093921, blocks_per_file, genesis);
        assert_eq!(range.start(), 9093921);
        assert_eq!(range.end(), 9593920); // 9093921 + 500000 - 1

        // Test with different blocks_per_file
        let genesis = 1000000;
        let blocks_per_file = 100000;
        
        let range = find_fixed_range_with_genesis(genesis, blocks_per_file, genesis);
        assert_eq!(range.start(), 1000000);
        assert_eq!(range.end(), 1099999);

        let range = find_fixed_range_with_genesis(1050000, blocks_per_file, genesis);
        assert_eq!(range.start(), 1000000);
        assert_eq!(range.end(), 1099999);

        let range = find_fixed_range_with_genesis(1100000, blocks_per_file, genesis);
        assert_eq!(range.start(), 1100000);
        assert_eq!(range.end(), 1199999);
    }
}
