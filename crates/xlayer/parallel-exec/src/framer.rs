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
    /// Optional auto-flush threshold: flush a frame when it reaches this many tasks.
    flush_threshold: Option<usize>,
    /// Whether the first flush has occurred.
    first_flushed: bool,
    /// Threshold for the first frame flush.
    first_flush_threshold: Option<usize>,
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
            flush_threshold: None,
            first_flushed: false,
            first_flush_threshold: None,
        }
    }

    /// Create a new `Framer` with a flush threshold.
    ///
    /// When a frame accumulates `threshold` tasks, it is automatically flushed.
    /// This enables pipeline overlap: flushed frames are returned immediately
    /// (via `add_returning_flushed`) and can be dispatched for execution while
    /// the Framer continues processing new transactions.
    pub fn with_flush_threshold(threshold: usize) -> Self {
        let max_frames = para_bloom::MAX_FRAMES;
        Self {
            bloom: ParaBloom::new(),
            frame_tasks: (0..max_frames).map(|_| Vec::new()).collect(),
            max_frames,
            completed_frames: Vec::new(),
            flush_threshold: Some(threshold),
            first_flushed: false,
            first_flush_threshold: None,
        }
    }

    /// Create a new `Framer` with early first-frame dispatch.
    pub fn with_early_dispatch(first_threshold: usize, threshold: usize) -> Self {
        let max_frames = para_bloom::MAX_FRAMES;
        Self {
            bloom: ParaBloom::new(),
            frame_tasks: (0..max_frames).map(|_| Vec::new()).collect(),
            max_frames,
            completed_frames: Vec::new(),
            flush_threshold: Some(threshold),
            first_flushed: false,
            first_flush_threshold: Some(first_threshold),
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

    /// Add a `SimResult` and return any frames that were flushed.
    ///
    /// Unlike `add()`, flushed frames are returned directly instead of being
    /// stored in `self.completed_frames`. This enables the async pipeline:
    /// the caller can dispatch flushed frames for execution immediately while
    /// continuing to frame new transactions.
    ///
    /// Auto-flush: if `flush_threshold` is set, a frame is also flushed when
    /// it accumulates that many tasks (similar to fafo's `can_flush()`).
    /// Add a **pre-built ExeTask** (may contain multiple txs) to the framer.
    pub fn add_task_returning_flushed(&mut self, task: ExeTask) -> Vec<Frame> {
        let mut flushed = Vec::new();
        let (all_reads, all_writes) = collect_read_write_hashes(&task.merged_crw_sets);

        let mask = self.bloom.get_dep_mask(&all_reads, &all_writes);

        let all_frames_mask =
            if self.max_frames >= 64 { u64::MAX } else { (1u64 << self.max_frames) - 1 };

        let frame_id = if mask & all_frames_mask == all_frames_mask {
            let tasks = std::mem::take(&mut self.frame_tasks[0]);
            if !tasks.is_empty() {
                flushed.push(Frame { tasks });
                self.first_flushed = true;
            }
            self.bloom.clear(0);
            0
        } else {
            mask.trailing_ones() as usize
        };

        if !self.first_flushed && frame_id > 0 && !self.frame_tasks[0].is_empty() {
            let tasks = std::mem::take(&mut self.frame_tasks[0]);
            flushed.push(Frame { tasks });
            self.bloom.clear(0);
            self.first_flushed = true;
        }

        self.bloom.add(frame_id, &all_reads, &all_writes);
        self.frame_tasks[frame_id].push(task);

        let effective_threshold = if !self.first_flushed {
            self.first_flush_threshold.or(self.flush_threshold)
        } else {
            self.flush_threshold
        };

        let should_flush = if let Some(threshold) = effective_threshold {
            self.frame_tasks[frame_id].len() >= threshold
        } else {
            false
        } || self.bloom.is_oversized(frame_id);

        if should_flush {
            let tasks = std::mem::take(&mut self.frame_tasks[frame_id]);
            if !tasks.is_empty() {
                flushed.push(Frame { tasks });
                self.first_flushed = true;
            }
            self.bloom.clear(frame_id);
        }

        flushed
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

    /// Add a single SimResult as its own ExeTask (backwards-compatible).
    pub fn add_returning_flushed(&mut self, sim_result: SimResult) -> Vec<Frame> {
        self.add_task_returning_flushed(ExeTask::new(sim_result))
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
    fn test_framer_early_dispatch_on_conflict() {
        // When a conflict causes a task to go to frame 1, frame 0 should be
        // flushed immediately (early dispatch) even before reaching threshold.
        let mut framer = Framer::with_early_dispatch(8, 64);

        let hash = [0xAAu8; 10];
        // tx 0: writes hash → frame 0
        let flushed = framer.add_returning_flushed(make_sim(0, vec![], vec![hash]));
        assert!(flushed.is_empty(), "no flush yet, only 1 task in frame 0");

        // tx 1: reads hash → conflict, goes to frame 1 → triggers early flush of frame 0
        let flushed = framer.add_returning_flushed(make_sim(1, vec![hash], vec![]));
        assert_eq!(flushed.len(), 1, "frame 0 should be flushed on conflict");
        assert_eq!(flushed[0].tasks.len(), 1);
        assert_eq!(flushed[0].tasks[0].sim_results[0].original_index, 0);
    }

    #[test]
    fn test_framer_first_flush_threshold() {
        // First frame should flush at first_threshold (2), not the normal threshold (64).
        let mut framer = Framer::with_early_dispatch(2, 64);

        // Add 2 non-conflicting tasks (different hashes) → both go to frame 0
        let flushed = framer.add_returning_flushed(make_sim(0, vec![], vec![[1u8; 10]]));
        assert!(flushed.is_empty());
        let flushed = framer.add_returning_flushed(make_sim(1, vec![], vec![[2u8; 10]]));
        assert_eq!(flushed.len(), 1, "should flush at first_threshold=2");
        assert_eq!(flushed[0].tasks.len(), 2);

        // After first flush, normal threshold (64) applies
        for i in 2..10 {
            let flushed =
                framer.add_returning_flushed(make_sim(i, vec![], vec![[(i as u8) + 10; 10]]));
            assert!(flushed.is_empty(), "should not flush at {i}, normal threshold is 64");
        }
    }

    #[test]
    fn test_framer_early_dispatch_only_once() {
        // After the first flush (conflict-triggered), subsequent frames use normal threshold.
        let mut framer = Framer::with_early_dispatch(8, 64);

        let hash = [0xBBu8; 10];
        // tx 0 writes hash → frame 0
        framer.add_returning_flushed(make_sim(0, vec![], vec![hash]));
        // tx 1 reads hash → conflict → frame 0 flushed early
        let flushed = framer.add_returning_flushed(make_sim(1, vec![hash], vec![]));
        assert_eq!(flushed.len(), 1);

        // Now add more non-conflicting tasks. They should NOT flush until reaching 64.
        for i in 2..10 {
            let flushed =
                framer.add_returning_flushed(make_sim(i, vec![], vec![[(i as u8) + 20; 10]]));
            assert!(flushed.is_empty(), "should not flush, first_flushed is true, threshold=64");
        }
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
