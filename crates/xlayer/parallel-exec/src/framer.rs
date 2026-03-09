//! Transaction framer for grouping non-conflicting transactions.
//!
//! Uses [`ParaBloom`] to detect conflicts and assigns transactions to frames.
//! Transactions within the same frame can be executed in parallel.
//!
//! The framer maintains up to [`MAX_FRAMES`](crate::para_bloom::MAX_FRAMES) concurrent
//! frames. Each incoming `SimResult` is assigned to the first non-conflicting frame.
//! When all frames conflict, the oldest frame is flushed to make room.

use crate::{
    crw_sets::ShortHash,
    para_bloom::{self, ParaBloom},
    task::{ExeTask, SimResult},
};

/// A frame containing non-conflicting tasks. All tasks in a frame
/// can be executed in parallel.
#[derive(Debug)]
pub struct Frame {
    /// Tasks within this frame (all non-conflicting with each other).
    pub tasks: Vec<ExeTask>,
}

/// Groups transactions into frames based on read/write conflict detection.
///
/// Uses `ParaBloom` to quickly detect conflicts. Transactions that don't
/// conflict with any existing frame are added to the first available frame.
/// When all frames conflict, the oldest frame is flushed.
#[derive(Debug)]
pub struct Framer {
    bloom: ParaBloom,
    /// Current tasks for each frame slot.
    frame_tasks: Vec<Vec<ExeTask>>,
    /// Maximum number of concurrent frames.
    max_frames: usize,
    /// Completed (flushed) frames, in order.
    completed_frames: Vec<Frame>,
}

impl Framer {
    /// Create a new `Framer` with the default maximum number of frames (64).
    pub fn new() -> Self {
        Self::with_max_frames(para_bloom::MAX_FRAMES)
    }

    /// Create a new `Framer` with a custom maximum number of frames.
    ///
    /// `max_frames` is capped at [`para_bloom::MAX_FRAMES`] (64) because the
    /// bloom filter uses a `u64` bitmask for conflict detection.
    pub fn with_max_frames(max_frames: usize) -> Self {
        let max_frames = max_frames.min(para_bloom::MAX_FRAMES);
        Self {
            bloom: ParaBloom::new(),
            frame_tasks: (0..max_frames).map(|_| Vec::new()).collect(),
            max_frames,
            completed_frames: Vec::new(),
        }
    }

    /// Add a `SimResult` to the appropriate frame.
    ///
    /// If all frames conflict, flushes the oldest frame first to make room.
    pub fn add(&mut self, sim_result: SimResult) {
        let (all_reads, all_writes) = collect_read_write_hashes(&sim_result.crw_sets);

        let mask = self.bloom.get_dep_mask(&all_reads, &all_writes);

        // Build a bitmask of all active frame slots.
        let all_frames_mask =
            if self.max_frames >= 64 { u64::MAX } else { (1u64 << self.max_frames) - 1 };

        let frame_id = if mask & all_frames_mask == all_frames_mask {
            // All frames conflict -- flush frame 0 (oldest) to make room.
            self.flush_frame(0);
            0
        } else {
            // trailing_ones() gives the index of the first zero bit,
            // i.e. the first non-conflicting frame.
            mask.trailing_ones() as usize
        };

        self.bloom.add(frame_id, &all_reads, &all_writes);

        let task = ExeTask::new(sim_result);
        self.frame_tasks[frame_id].push(task);
    }

    /// Flush a specific frame, moving its tasks to `completed_frames`.
    fn flush_frame(&mut self, frame_id: usize) {
        let tasks = std::mem::take(&mut self.frame_tasks[frame_id]);
        if !tasks.is_empty() {
            self.completed_frames.push(Frame { tasks });
        }
        self.bloom.clear(frame_id);
    }

    /// Finish framing: flush all remaining frames and return them in order.
    pub fn finish(mut self) -> Vec<Frame> {
        for i in 0..self.max_frames {
            self.flush_frame(i);
        }
        self.completed_frames
    }

    /// Number of currently active (non-empty) frames.
    pub fn active_frame_count(&self) -> usize {
        self.frame_tasks.iter().filter(|f| !f.is_empty()).count()
    }
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect all read and write hashes from a `CrwSets` into flat vectors.
///
/// Combines account and storage reads/writes respectively, since the bloom
/// filter does not distinguish between account-level and slot-level accesses.
fn collect_read_write_hashes(crw: &crate::crw_sets::CrwSets) -> (Vec<ShortHash>, Vec<ShortHash>) {
    let all_reads: Vec<ShortHash> =
        crw.account_reads.iter().chain(crw.storage_reads.iter()).copied().collect();
    let all_writes: Vec<ShortHash> =
        crw.account_writes.iter().chain(crw.storage_writes.iter()).copied().collect();
    (all_reads, all_writes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crw_sets::CrwSets;

    fn make_sim(index: usize, reads: Vec<[u8; 10]>, writes: Vec<[u8; 10]>) -> SimResult {
        SimResult {
            crw_sets: CrwSets {
                account_reads: reads,
                account_writes: writes,
                storage_reads: vec![],
                storage_writes: vec![],
            },
            original_index: index,
            success: true,
        }
    }

    #[test]
    fn test_framer_no_conflicts() {
        // Transactions with disjoint read sets should all land in frame 0
        // because read-read does not conflict.
        let mut framer = Framer::with_max_frames(4);
        framer.add(make_sim(0, vec![[1u8; 10]], vec![]));
        framer.add(make_sim(1, vec![[2u8; 10]], vec![]));
        framer.add(make_sim(2, vec![[3u8; 10]], vec![]));

        // All should be in frame 0 (reads don't conflict with reads)
        assert_eq!(framer.frame_tasks[0].len(), 3);

        let frames = framer.finish();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tasks.len(), 3);
    }

    #[test]
    fn test_framer_with_conflicts() {
        // tx A writes slot X, tx B reads slot X -> different frames
        let mut framer = Framer::with_max_frames(4);

        let hash_x = [0xAAu8; 10];
        // tx 0 writes hash_x
        framer.add(make_sim(0, vec![], vec![hash_x]));
        // tx 1 reads hash_x -> conflicts with frame 0's writes
        framer.add(make_sim(1, vec![hash_x], vec![]));

        assert_eq!(framer.frame_tasks[0].len(), 1);
        assert_eq!(framer.frame_tasks[1].len(), 1);

        let frames = framer.finish();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn test_framer_multiple_frames() {
        // Three groups of conflicting transactions on the same hash.
        let mut framer = Framer::with_max_frames(4);

        let hash = [0xBBu8; 10];
        // Each tx writes the same hash, so each must go to a different frame.
        framer.add(make_sim(0, vec![], vec![hash]));
        framer.add(make_sim(1, vec![], vec![hash]));
        framer.add(make_sim(2, vec![], vec![hash]));

        assert_eq!(framer.frame_tasks[0].len(), 1);
        assert_eq!(framer.frame_tasks[1].len(), 1);
        assert_eq!(framer.frame_tasks[2].len(), 1);
        assert_eq!(framer.active_frame_count(), 3);

        let frames = framer.finish();
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn test_framer_flush_on_full() {
        // With max_frames=2, the third conflicting tx forces a flush.
        let mut framer = Framer::with_max_frames(2);

        let hash = [0xCCu8; 10];
        framer.add(make_sim(0, vec![], vec![hash]));
        framer.add(make_sim(1, vec![], vec![hash]));

        // Both frame slots are now occupied with conflicting writes.
        assert_eq!(framer.active_frame_count(), 2);

        // This add should flush frame 0 (oldest), then place tx 2 in frame 0.
        framer.add(make_sim(2, vec![], vec![hash]));

        // Frame 0 was flushed (1 completed), then tx 2 placed in frame 0.
        assert_eq!(framer.completed_frames.len(), 1);
        assert_eq!(framer.completed_frames[0].tasks.len(), 1);
        assert_eq!(framer.completed_frames[0].tasks[0].sim_results[0].original_index, 0);

        let frames = framer.finish();
        // 1 previously flushed + 2 remaining frames
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn test_framer_finish_returns_all() {
        let mut framer = Framer::with_max_frames(4);

        // Add non-conflicting write txs (different hashes) to fill multiple frames,
        // then verify finish returns everything.
        framer.add(make_sim(0, vec![], vec![[1u8; 10]]));
        framer.add(make_sim(1, vec![], vec![[2u8; 10]]));

        // These don't conflict, so both in frame 0
        // Actually writes to different hashes don't conflict, so both in frame 0.
        let frames = framer.finish();
        // All in one frame since hashes are disjoint
        assert!(!frames.is_empty());
        let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
        assert_eq!(total_tasks, 2);
    }

    #[test]
    fn test_framer_preserves_original_index() {
        let mut framer = Framer::with_max_frames(4);

        framer.add(make_sim(42, vec![[1u8; 10]], vec![]));
        framer.add(make_sim(99, vec![[2u8; 10]], vec![]));
        framer.add(make_sim(7, vec![[3u8; 10]], vec![]));

        let frames = framer.finish();
        let mut indices: Vec<usize> = frames
            .iter()
            .flat_map(|f| f.tasks.iter())
            .flat_map(|t| t.sim_results.iter())
            .map(|sr| sr.original_index)
            .collect();
        indices.sort();
        assert_eq!(indices, vec![7, 42, 99]);
    }
}
