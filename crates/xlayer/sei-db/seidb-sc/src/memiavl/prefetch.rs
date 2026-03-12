//! Snapshot prefetch utilities for cold-start page cache warming.
//!
//! On cold starts, the OS page cache is empty and snapshot file access becomes
//! random I/O heavy. By sequentially reading snapshot files before the tree is
//! used, we trigger kernel readahead and fill the page cache, eliminating the
//! majority of random I/O during replay.
//!
//! Ported from the Go implementation in `sei-chain/sei-db/state_db/sc/memiavl/snapshot.go`.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use tracing::{debug, info, warn};

/// Chunk size for sequential reads (4 MiB).
const PREFETCH_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Snapshot file names in the order they are prefetched.
const SNAPSHOT_FILES: [&str; 4] = ["metadata", "nodes", "leaves", "kvs"];

/// Sequentially read a file to fill the OS page cache.
///
/// The data is discarded — this only triggers kernel readahead so that
/// subsequent mmap access hits pages already resident in memory.
///
/// Returns the total number of bytes read.
pub fn sequential_read_and_fill_page_cache(path: &Path) -> io::Result<u64> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size == 0 {
        debug!(path = %path.display(), "skipping empty file for prefetch");
        return Ok(0);
    }

    let size_mib = file_size as f64 / (1024.0 * 1024.0);
    debug!(path = %path.display(), size_mib, "starting to prefetch file");

    let total_read = AtomicU64::new(0);
    let mut reader = io::BufReader::with_capacity(PREFETCH_CHUNK_SIZE, file);
    let mut buf = vec![0u8; PREFETCH_CHUNK_SIZE];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total_read.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    let bytes = total_read.load(Ordering::Relaxed);
    let elapsed_mib = bytes as f64 / (1024.0 * 1024.0);
    debug!(
        path = %path.display(),
        size_mib = elapsed_mib,
        "completed prefetching file"
    );

    Ok(bytes)
}

/// Prefetch all snapshot files (metadata, nodes, leaves, kvs) sequentially.
///
/// Call this before using a snapshot to warm the page cache on cold starts.
/// Files that do not exist are silently skipped.
pub fn prefetch_snapshot(dir: &Path) {
    let start = std::time::Instant::now();

    for name in &SNAPSHOT_FILES {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        match sequential_read_and_fill_page_cache(&path) {
            Ok(bytes) => {
                info!(file = *name, bytes, "prefetched snapshot file");
            }
            Err(e) => {
                warn!(
                    file = *name,
                    error = %e,
                    "failed to prefetch snapshot file"
                );
            }
        }
    }

    let elapsed = start.elapsed();
    info!(
        dir = %dir.display(),
        elapsed_secs = elapsed.as_secs_f64(),
        "prefetch snapshot completed"
    );
}

/// Determine whether a snapshot directory should be preloaded.
///
/// The Go implementation uses `mincore()` to check page-cache residency and
/// skips prefetch when the ratio exceeds `threshold`. The Rust version is
/// simplified: it always returns `true` because `mincore` requires the data
/// to already be mmap-ed with a stable pointer, which does not fit the
/// current open sequence (prefetch happens *before* mmap validation).
///
/// A full implementation could mmap the file temporarily, call
/// `libc::mincore`, and count resident pages.
pub fn should_preload_tree(_dir: &Path, _threshold: f64) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_sequential_read_fill_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("testfile");

        // Write 1 MiB of data
        let data = vec![0xABu8; 1024 * 1024];
        fs::write(&file_path, &data).unwrap();

        let bytes_read = sequential_read_and_fill_page_cache(&file_path).unwrap();
        assert_eq!(bytes_read, data.len() as u64);
    }

    #[test]
    fn test_sequential_read_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("empty");
        fs::write(&file_path, &[]).unwrap();

        let bytes_read = sequential_read_and_fill_page_cache(&file_path).unwrap();
        assert_eq!(bytes_read, 0);
    }

    #[test]
    fn test_sequential_read_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("no_such_file");

        let result = sequential_read_and_fill_page_cache(&file_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_sequential_read_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("large");

        // Write 10 MiB — larger than PREFETCH_CHUNK_SIZE to exercise multi-chunk reads
        let data = vec![0xCDu8; 10 * 1024 * 1024];
        fs::write(&file_path, &data).unwrap();

        let bytes_read = sequential_read_and_fill_page_cache(&file_path).unwrap();
        assert_eq!(bytes_read, data.len() as u64);
    }

    #[test]
    fn test_prefetch_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Create all 4 snapshot files with known sizes
        fs::write(d.join("metadata"), vec![0u8; 12]).unwrap();
        fs::write(d.join("nodes"), vec![0u8; 512]).unwrap();
        fs::write(d.join("leaves"), vec![0u8; 256]).unwrap();
        fs::write(d.join("kvs"), vec![0u8; 1024]).unwrap();

        // Should not panic or error
        prefetch_snapshot(d);
    }

    #[test]
    fn test_prefetch_snapshot_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Only create some files — missing ones should be silently skipped
        fs::write(d.join("metadata"), vec![0u8; 12]).unwrap();
        fs::write(d.join("kvs"), vec![0u8; 64]).unwrap();

        // Should not panic or error even with missing nodes/leaves
        prefetch_snapshot(d);
    }

    #[test]
    fn test_prefetch_snapshot_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // No files at all
        prefetch_snapshot(dir.path());
    }

    #[test]
    fn test_should_preload_tree() {
        let dir = tempfile::tempdir().unwrap();
        // Simplified version always returns true
        assert!(should_preload_tree(dir.path(), 0.5));
        assert!(should_preload_tree(dir.path(), 0.0));
        assert!(should_preload_tree(dir.path(), 1.0));
    }
}
