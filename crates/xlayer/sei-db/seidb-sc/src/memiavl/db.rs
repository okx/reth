use crate::memiavl::{
    multitree::{read_metadata, MultiTree},
    prefetch::prefetch_snapshot,
    tree::Tree,
};
use crossbeam_channel::{Receiver, TryRecvError};
use fs4::fs_std::FileExt;
use seidb_common::{
    config::MemIavlConfig,
    error::{join_errors, Result, SeiDbError},
    path::get_changelog_path,
    snapshot_dir::{
        atomic_remove_dir, current_path, current_version, remove_tmp_dirs, seek_snapshot,
        snapshot_name, traverse_snapshots, update_current_symlink,
    },
};
use seidb_proto::{ChangelogEntry, CommitInfo, NamedChangeSet, TreeNameUpgrade};
use seidb_traits::wal::Wal;
use seidb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::{
    fs,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::Instant,
};

const LOCK_FILE_NAME: &str = "LOCK";

/// MemIAVL DB layer wrapping [`MultiTree`] with WAL, snapshot management,
/// and lifecycle control.
///
/// The on-disk layout:
/// ```text
/// <dir>/
///   current -> snapshot-N
///   snapshot-N/
///     bank/  (kvs, nodes, leaves, metadata)
///     acc/
///     ... other stores
///     __metadata
///   changelog/
///     ... WAL segment files
///   LOCK
/// ```
pub struct DB {
    multi_tree: MultiTree,
    dir: PathBuf,
    file_lock: Option<fs::File>,
    read_only: bool,

    // WAL
    stream_handler: Option<ChangelogWal>,
    pending_log_entry: ChangelogEntry,
    wal_index_delta: i64,

    // Snapshot management
    snapshot_keep_recent: u32,
    snapshot_interval: u32,
    snapshot_min_time_interval: u32,
    last_snapshot_time: Option<Instant>,

    // Background snapshot rewrite state.
    // When a background snapshot is in progress, this holds:
    // - A receiver that will deliver the result (Ok(snapshot_dir) or Err)
    // - A JoinHandle for the background thread (used in close() to wait for completion)
    snapshot_result_rx: Option<Receiver<Result<PathBuf>>>,
    snapshot_rewrite_handle: Option<JoinHandle<()>>,

    config: MemIavlConfig,
}

impl DB {
    /// Open or create a MemIAVL database.
    ///
    /// Follows the Go `OpenDB` logic:
    /// 1. Acquire file lock (unless read-only)
    /// 2. Remove leftover temp directories
    /// 3. Determine snapshot directory (current symlink or seek to target_version)
    /// 4. Load MultiTree from snapshot
    /// 5. Open WAL
    /// 6. Compute wal_index_delta
    /// 7. Replay WAL entries after snapshot version
    /// 8. Validate target_version
    pub fn open(
        dir: &Path,
        target_version: i64,
        config: &MemIavlConfig,
        read_only: bool,
    ) -> Result<Self> {
        // Ensure directory exists
        fs::create_dir_all(dir)?;

        // Acquire file lock unless read-only
        let file_lock = if !read_only {
            let lock_path = dir.join(LOCK_FILE_NAME);
            let lock_file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)?;
            lock_file
                .try_lock_exclusive()
                .map_err(|e| SeiDbError::Other(format!("failed to lock db: {e}")))?;

            // Clean up temporary directories from interrupted snapshot rewrites
            remove_tmp_dirs(dir)?;

            Some(lock_file)
        } else {
            None
        };

        // Determine which snapshot to load
        let snapshot_dir_name = if target_version > 0 {
            match seek_snapshot(dir, target_version)? {
                Some(v) => snapshot_name(v),
                None => {
                    return Err(SeiDbError::Other(format!(
                        "target version is pruned: {target_version}"
                    )));
                }
            }
        } else {
            // Use "current" symlink
            "current".to_string()
        };

        let snapshot_path = dir.join(&snapshot_dir_name);

        // If the snapshot path doesn't exist and this is a fresh DB, initialize it
        if !snapshot_path.exists() && snapshot_dir_name == "current" {
            init_empty_db(dir, 0)?;
        }

        // Prefetch snapshot files into OS page cache before mmap.
        // Eliminates random I/O during cold-start replay by triggering
        // sequential kernel readahead on all snapshot data files.
        prefetch_snapshot(&snapshot_path);

        let mut multi_tree = MultiTree::load(&snapshot_path)?;

        // Configure snapshot rate limiter from config to prevent page cache eviction
        multi_tree.set_snapshot_rate_limiter(crate::memiavl::rate_limiter::RateLimiter::new(
            config.snapshot_write_rate_mbps as u32,
        ));
        multi_tree.set_snapshot_writer_limit(config.snapshot_writer_limit);

        // Open WAL
        let changelog_path = get_changelog_path(dir);
        let wal_config = seidb_common::config::WalConfig {
            write_buffer_size: config.async_commit_buffer,
            fsync_enabled: false,
            ..Default::default()
        };
        let stream_handler = new_changelog_wal(wal_config, &changelog_path)?;

        // Compute WAL index delta
        let (wal_index_delta, wal_has_entries) = compute_wal_index_delta(&stream_handler)?;

        // If WAL is empty, set delta so first entry aligns with next version
        let wal_index_delta = if !wal_has_entries {
            multi_tree.working_commit_info().version - 1
        } else {
            wal_index_delta
        };

        // Enable per-tree async apply when async_commit_buffer > 0,
        // matching Go's asyncCommit behaviour.
        if config.async_commit_buffer > 0 {
            multi_tree.set_async_apply(true);
        }

        let mut db = DB {
            multi_tree,
            dir: dir.to_path_buf(),
            file_lock,
            read_only,
            stream_handler: Some(stream_handler),
            pending_log_entry: ChangelogEntry::default(),
            wal_index_delta,
            snapshot_keep_recent: config.snapshot_keep_recent,
            snapshot_interval: config.snapshot_interval,
            snapshot_min_time_interval: config.snapshot_min_time_interval,
            last_snapshot_time: None,
            snapshot_result_rx: None,
            snapshot_rewrite_handle: None,
            config: config.clone(),
        };

        // Replay WAL to catch up
        if wal_has_entries && (target_version == 0 || target_version > db.multi_tree.version()) {
            db.catchup_wal(target_version)?;
        }

        // If target_version is specified and we need to truncate WAL
        if target_version > 0 && wal_has_entries {
            let current_ver = db.multi_tree.version();
            if current_ver > target_version {
                return Err(SeiDbError::Other(format!(
                    "target version {target_version} is behind current version {current_ver}"
                )));
            }
        }

        Ok(db)
    }

    /// Close the DB, releasing WAL, MultiTree resources, and file lock.
    pub fn close(&mut self) -> Result<()> {
        let mut errs = Vec::new();

        // Wait for any in-progress background snapshot rewrite to finish.
        // We must do this before closing the WAL since the background thread
        // may reference WAL data indirectly through the result channel.
        if let Some(handle) = self.snapshot_rewrite_handle.take() {
            let _ = handle.join();
        }
        // Drain the result channel (discard whatever the background thread produced)
        self.snapshot_result_rx = None;

        // Close WAL
        if let Some(ref mut wal) = self.stream_handler &&
            let Err(e) = wal.close()
        {
            errs.push(e);
        }
        self.stream_handler = None;

        // Close MultiTree
        if let Err(e) = self.multi_tree.close() {
            errs.push(e);
        }

        // Release file lock
        if let Some(ref lock) = self.file_lock &&
            let Err(e) = lock.unlock()
        {
            errs.push(SeiDbError::Io(e));
        }
        self.file_lock = None;

        if let Some(err) = join_errors(errs) {
            Err(err)
        } else {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Version / state queries
    // -----------------------------------------------------------------------

    /// Return the current committed version.
    pub fn version(&self) -> i64 {
        self.multi_tree.version()
    }

    /// Return the committed version (same as `version`).
    pub fn committed_version(&self) -> i64 {
        self.multi_tree.version()
    }

    /// Look up a tree by name.
    pub fn tree_by_name(&self, name: &str) -> Option<&Tree> {
        self.multi_tree.tree_by_name(name)
    }

    /// Return all named trees, ordered by name.
    pub fn trees(&self) -> &[crate::memiavl::multitree::NamedTree] {
        self.multi_tree.trees()
    }

    /// Return a reference to the last commit info.
    pub fn last_commit_info(&self) -> &CommitInfo {
        self.multi_tree.last_commit_info()
    }

    /// Build commit info from the current (possibly uncommitted) tree state.
    pub fn working_commit_info(&self) -> CommitInfo {
        self.multi_tree.working_commit_info()
    }

    // -----------------------------------------------------------------------
    // ChangeSets / Upgrades
    // -----------------------------------------------------------------------

    /// Apply named change sets to the corresponding trees.
    ///
    /// Changes are also accumulated in `pending_log_entry` for the next WAL write.
    pub fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()> {
        if self.read_only {
            return Err(SeiDbError::Other("db is read-only".into()));
        }
        if cs.is_empty() {
            return Ok(());
        }
        // Accumulate into pending log entry only when WAL is enabled, and reuse the
        // allocation to avoid copying 10–20K kv pairs every block.
        if self.stream_handler.is_some() {
            self.pending_log_entry.changesets.clear();
            self.pending_log_entry.changesets.extend_from_slice(cs);
        }
        self.multi_tree.apply_change_sets(cs)
    }

    /// Apply tree name upgrades (add, delete, rename).
    ///
    /// Changes are also accumulated in `pending_log_entry` for the next WAL write.
    pub fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()> {
        if self.read_only {
            return Err(SeiDbError::Other("db is read-only".into()));
        }
        if !upgrades.is_empty() && self.stream_handler.is_some() {
            self.pending_log_entry.upgrades.extend(upgrades.iter().cloned());
        }
        self.multi_tree.apply_upgrades(upgrades)
    }

    // -----------------------------------------------------------------------
    // Commit
    // -----------------------------------------------------------------------

    /// Commit the current in-memory tree state and write to WAL.
    ///
    /// 1. Save version in-memory
    /// 2. Write pending changelog entry to WAL
    /// 3. Clear pending entry for next block
    /// 4. Check if a background snapshot has completed (non-blocking)
    /// 5. Check if a new snapshot rewrite should be triggered
    pub fn commit(&mut self) -> Result<i64> {
        if self.read_only {
            return Err(SeiDbError::Other("db is read-only".into()));
        }

        // Save version in-memory (no persistence yet)
        let (version, _commit_info) = self.multi_tree.save_version(true)?;

        // Write to WAL
        if let Some(ref wal) = self.stream_handler {
            let mut entry = std::mem::take(&mut self.pending_log_entry);
            entry.version = version;
            wal.write(entry)?;
        }
        // Clear pending entry for next block
        self.pending_log_entry = ChangelogEntry::default();

        // Check if a background snapshot has completed and apply it
        self.check_background_snapshot_rewrite()?;

        // Check if a new snapshot rewrite should be triggered
        self.rewrite_if_applicable(version);

        Ok(version)
    }

    // -----------------------------------------------------------------------
    // WAL index conversion
    // -----------------------------------------------------------------------

    /// Convert a version to its corresponding WAL index.
    fn version_to_wal_index(&self, version: i64) -> u64 {
        let index = version - self.wal_index_delta;
        if index <= 0 {
            0
        } else {
            index as u64
        }
    }

    /// Convert a WAL index to its corresponding version.
    #[allow(dead_code)]
    fn wal_index_to_version(&self, index: u64) -> i64 {
        index as i64 + self.wal_index_delta
    }

    /// Return the WAL index delta.
    pub fn get_wal_index_delta(&self) -> i64 {
        self.wal_index_delta
    }

    /// Return a reference to the WAL, if open.
    pub fn get_wal(&self) -> Option<&ChangelogWal> {
        self.stream_handler.as_ref()
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    /// Write the current version as a snapshot and update the `current` symlink.
    ///
    /// This is a **synchronous** snapshot write -- it blocks until the snapshot
    /// is fully written, renamed, and the symlink updated.
    pub fn rewrite_snapshot(&mut self) -> Result<()> {
        if self.read_only {
            return Err(SeiDbError::Other("db is read-only".into()));
        }

        let version = self.multi_tree.version();
        let snap_dir_name = snapshot_name(version);
        let target_path = self.dir.join(&snap_dir_name);

        // Skip if snapshot already exists
        if target_path.exists() && target_path.is_dir() {
            return Ok(());
        }

        let tmp_dir_name = format!("{snap_dir_name}-tmp");
        let tmp_path = self.dir.join(&tmp_dir_name);

        // Write snapshot to temp directory
        match self.multi_tree.write_snapshot(&tmp_path) {
            Ok(()) => {}
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp_path);
                return Err(e);
            }
        }

        // Rename temp to final
        if let Err(e) = fs::rename(&tmp_path, &target_path) {
            let _ = fs::remove_dir_all(&tmp_path);
            return Err(SeiDbError::Io(e));
        }

        // Update current symlink
        update_current_symlink(&self.dir, &snap_dir_name)?;

        // Update last snapshot time
        self.last_snapshot_time = Some(Instant::now());

        // Prune old snapshots
        self.prune_snapshots();

        Ok(())
    }

    /// Spawn a background thread to write a snapshot from a CoW clone of the DB.
    ///
    /// The background thread:
    /// 1. Creates a read-only copy of the current MultiTree (O(1) via Arc)
    /// 2. Writes the snapshot to a temp directory, renames it, updates the symlink
    /// 3. Sends the snapshot directory path back via a channel
    ///
    /// The main thread checks for completion in [`check_background_snapshot_rewrite`]
    /// (called at the start of each [`commit`]).
    pub fn rewrite_snapshot_background(&mut self) -> Result<()> {
        if self.read_only {
            return Err(SeiDbError::Other("db is read-only".into()));
        }

        if self.snapshot_result_rx.is_some() {
            return Err(SeiDbError::Other(
                "there's another ongoing snapshot rewriting process".into(),
            ));
        }

        // Update snapshot timestamp at start (not at completion) to prevent
        // rapid re-triggering while the background thread is still running.
        self.last_snapshot_time = Some(Instant::now());

        let copy = self.copy();
        let dir = self.dir.clone();

        let (tx, rx) = crossbeam_channel::bounded(1);
        self.snapshot_result_rx = Some(rx);

        let handle = std::thread::spawn(move || {
            let result = write_snapshot_in_background(copy, &dir);
            // Send result; if the receiver was dropped (DB closed), that's fine.
            let _ = tx.send(result);
        });
        self.snapshot_rewrite_handle = Some(handle);

        Ok(())
    }

    /// Non-blocking check for background snapshot completion.
    ///
    /// Called during each [`commit`]. If the background thread has finished:
    /// - On success: reload the MultiTree from the new snapshot, catchup WAL, swap in the new tree,
    ///   and prune old snapshots.
    /// - On failure: log the error, prune old snapshots, and continue.
    fn check_background_snapshot_rewrite(&mut self) -> Result<()> {
        let rx = match self.snapshot_result_rx.as_ref() {
            Some(rx) => rx,
            None => return Ok(()), // no background snapshot in progress
        };

        match rx.try_recv() {
            Err(TryRecvError::Empty) => {
                // Background thread still running -- nothing to do.
                Ok(())
            }
            Err(TryRecvError::Disconnected) => {
                // Thread panicked or channel was dropped without sending.
                self.snapshot_result_rx = None;
                if let Some(handle) = self.snapshot_rewrite_handle.take() {
                    let _ = handle.join();
                }
                self.prune_snapshots();
                Err(SeiDbError::Other(
                    "background snapshot rewrite thread terminated unexpectedly".into(),
                ))
            }
            Ok(Err(e)) => {
                // Background snapshot failed.
                self.snapshot_result_rx = None;
                if let Some(handle) = self.snapshot_rewrite_handle.take() {
                    let _ = handle.join();
                }
                self.prune_snapshots();
                Err(SeiDbError::Other(format!("background snapshot rewriting failed: {e}")))
            }
            Ok(Ok(snap_dir)) => {
                // Background snapshot succeeded -- reload from the new snapshot.
                self.snapshot_result_rx = None;
                if let Some(handle) = self.snapshot_rewrite_handle.take() {
                    let _ = handle.join();
                }

                self.reload_multi_tree(&snap_dir)?;
                self.prune_snapshots();
                Ok(())
            }
        }
    }

    /// Load a new MultiTree from `snap_dir`, catchup WAL to the current
    /// version, and atomically swap it in as the active tree.
    fn reload_multi_tree(&mut self, snap_dir: &Path) -> Result<()> {
        let mut new_mt = MultiTree::load(snap_dir)?;

        // WAL catchup: replay all entries from the snapshot version to current.
        // The new MultiTree was written at some past version; we need to bring
        // it up to date with the commits that happened while the snapshot was
        // being written in the background.
        if let Some(ref wal) = self.stream_handler {
            let snap_version = new_mt.version();
            let start_wal_index = self.version_to_wal_index(snap_version + 1);
            let last_offset = wal.last_offset()?;

            if last_offset > 0 && start_wal_index <= last_offset {
                let mut entries = Vec::new();
                wal.replay(start_wal_index, last_offset, &mut |_idx, entry: ChangelogEntry| {
                    entries.push(entry);
                    Ok(())
                })?;

                for entry in entries {
                    if !entry.upgrades.is_empty() {
                        new_mt.apply_upgrades(&entry.upgrades)?;
                    }
                    if !entry.changesets.is_empty() {
                        new_mt.apply_change_sets(&entry.changesets)?;
                    }
                    new_mt.save_version(true)?;
                }
            }
        }

        // Swap the old multi_tree with the new one.
        // The old MultiTree's resources (mmap'd files etc.) are dropped here.
        let mut old_mt = std::mem::replace(&mut self.multi_tree, new_mt);
        let _ = old_mt.close();

        Ok(())
    }

    /// Check if a snapshot rewrite should be triggered based on the current height.
    ///
    /// Three conditions must all be true:
    /// 1. `snapshot_interval > 0`
    /// 2. `height % snapshot_interval == 0`
    /// 3. `last_snapshot_time` is None or elapsed >= `snapshot_min_time_interval`
    fn rewrite_if_applicable(&mut self, height: i64) {
        // Don't trigger if a background snapshot is already in progress
        if self.snapshot_result_rx.is_some() {
            return;
        }

        if self.snapshot_interval == 0 || height <= 0 {
            return;
        }

        if height % (self.snapshot_interval as i64) != 0 {
            return;
        }

        // Check minimum time interval
        if let Some(last_time) = self.last_snapshot_time {
            let elapsed = last_time.elapsed().as_secs() as u32;
            if elapsed < self.snapshot_min_time_interval {
                return;
            }
        }

        // Trigger background snapshot rewrite
        if let Err(_e) = self.rewrite_snapshot_background() {
            // Log error but don't fail the commit
        }
    }

    /// Remove old snapshots, keeping only `snapshot_keep_recent` recent ones
    /// (excluding the current/latest).
    fn prune_snapshots(&self) {
        let cur_version = match current_version(&self.dir) {
            Ok(v) => v,
            Err(_) => return,
        };

        let mut counter = self.snapshot_keep_recent;
        let _ = traverse_snapshots(&self.dir, false, |version| {
            if version >= cur_version {
                // Skip current and any newer snapshots
                return Ok(true);
            }

            if counter > 0 {
                counter -= 1;
                return Ok(true);
            }

            let snap_path = self.dir.join(snapshot_name(version));
            let _ = atomic_remove_dir(&snap_path);
            Ok(true)
        });
    }

    // -----------------------------------------------------------------------
    // Copy (for background snapshot)
    // -----------------------------------------------------------------------

    /// Create a read-only copy of the DB that shares tree nodes via Arc.
    /// The copy has no WAL and no file lock.
    pub fn copy(&mut self) -> Self {
        let mtree_copy = self.multi_tree.copy();
        DB {
            multi_tree: mtree_copy,
            dir: self.dir.clone(),
            file_lock: None,
            read_only: true,
            stream_handler: None,
            pending_log_entry: ChangelogEntry::default(),
            wal_index_delta: self.wal_index_delta,
            snapshot_keep_recent: self.snapshot_keep_recent,
            snapshot_interval: self.snapshot_interval,
            snapshot_min_time_interval: self.snapshot_min_time_interval,
            last_snapshot_time: None,
            snapshot_result_rx: None,
            snapshot_rewrite_handle: None,
            config: self.config.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // WAL catchup
    // -----------------------------------------------------------------------

    /// Replay WAL entries from the snapshot version to the latest (or target_version).
    fn catchup_wal(&mut self, target_version: i64) -> Result<()> {
        let wal = match self.stream_handler.as_ref() {
            Some(w) => w,
            None => return Ok(()),
        };

        let snapshot_version = self.multi_tree.version();
        let start_wal_index = self.version_to_wal_index(snapshot_version + 1);

        let last_offset = wal.last_offset()?;
        if last_offset == 0 || start_wal_index > last_offset {
            return Ok(());
        }

        let end_wal_index = if target_version > 0 {
            let target_idx = self.version_to_wal_index(target_version);
            std::cmp::min(target_idx, last_offset)
        } else {
            last_offset
        };

        if start_wal_index > end_wal_index {
            return Ok(());
        }

        // Collect entries first to avoid borrowing issues
        let mut entries = Vec::new();
        wal.replay(start_wal_index, end_wal_index, &mut |_idx, entry: ChangelogEntry| {
            entries.push(entry);
            Ok(())
        })?;

        for entry in entries {
            if !entry.upgrades.is_empty() {
                self.multi_tree.apply_upgrades(&entry.upgrades)?;
            }
            if !entry.changesets.is_empty() {
                self.multi_tree.apply_change_sets(&entry.changesets)?;
            }
            self.multi_tree.save_version(true)?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Static helpers
    // -----------------------------------------------------------------------

    /// Find the latest version without loading the entire DB.
    ///
    /// Reads the snapshot metadata from the `current` symlink.
    pub fn get_latest_version(dir: &Path) -> Result<i64> {
        let cur = current_path(dir);
        match read_metadata(&cur) {
            Ok(metadata) => {
                let commit_info = metadata.commit_info.unwrap_or_default();
                Ok(commit_info.version)
            }
            Err(e) => {
                // If no current symlink exists, version is 0
                if matches!(&e, SeiDbError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound)
                {
                    Ok(0)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Find the earliest snapshot version.
    pub fn get_earliest_version(root: &Path) -> Result<i64> {
        let mut found: Option<i64> = None;
        traverse_snapshots(root, true, |version| {
            found = Some(version);
            Ok(false) // stop at first
        })?;
        match found {
            Some(v) => Ok(v),
            None => Err(SeiDbError::Other("empty memiavl db".into())),
        }
    }

    // -----------------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------------

    /// Set the initial version for all trees.
    pub fn set_initial_version(&mut self, v: u32) {
        self.multi_tree.set_initial_version(v);
    }

    /// Whether the DB is read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Compute the constant delta between version and WAL index.
///
/// Since both are strictly contiguous, reading one entry is sufficient.
/// Returns `(delta, has_entries)`.
fn compute_wal_index_delta(wal: &ChangelogWal) -> Result<(i64, bool)> {
    let first_index = wal.first_offset()?;
    if first_index == 0 {
        return Ok((0, false)); // empty WAL
    }

    // Read just the first entry
    let entry = wal.read_at(first_index)?;
    let delta = entry.version - first_index as i64;
    Ok((delta, true))
}

/// Initialize an empty DB with a version-0 snapshot and `current` symlink.
fn init_empty_db(dir: &Path, initial_version: u32) -> Result<()> {
    let mt = MultiTree::new_empty(initial_version);
    let snap_dir_name = snapshot_name(0);
    let snap_path = dir.join(&snap_dir_name);
    mt.write_snapshot(&snap_path)?;
    update_current_symlink(dir, &snap_dir_name)
}

/// Write a snapshot from a read-only DB copy in a background thread.
///
/// This function is called inside `std::thread::spawn`. It performs the full
/// snapshot write (temp dir, write, rename, symlink update) and returns the
/// final snapshot directory path on success.
fn write_snapshot_in_background(db_copy: DB, dir: &Path) -> Result<PathBuf> {
    let version = db_copy.multi_tree.version();
    let snap_dir_name = snapshot_name(version);
    let target_path = dir.join(&snap_dir_name);

    // Skip if snapshot already exists
    if target_path.exists() && target_path.is_dir() {
        return Ok(target_path);
    }

    let tmp_dir_name = format!("{snap_dir_name}-tmp");
    let tmp_path = dir.join(&tmp_dir_name);

    // Write snapshot to temp directory
    match db_copy.multi_tree.write_snapshot(&tmp_path) {
        Ok(()) => {}
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp_path);
            return Err(e);
        }
    }

    // Rename temp to final
    if let Err(e) = fs::rename(&tmp_path, &target_path) {
        let _ = fs::remove_dir_all(&tmp_path);
        return Err(SeiDbError::Io(e));
    }

    // Update current symlink
    update_current_symlink(dir, &snap_dir_name)?;

    Ok(target_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_proto::{ChangeSet, KvPair};

    fn default_config() -> MemIavlConfig {
        MemIavlConfig {
            async_commit_buffer: 0,
            snapshot_keep_recent: 2,
            snapshot_interval: 0,
            snapshot_min_time_interval: 0,
            ..Default::default()
        }
    }

    fn make_named_changeset(name: &str, pairs: Vec<KvPair>) -> NamedChangeSet {
        NamedChangeSet { name: name.to_string(), changeset: Some(ChangeSet { pairs }) }
    }

    fn make_kv_pair(key: &[u8], value: &[u8]) -> KvPair {
        KvPair { delete: false, key: key.to_vec(), value: value.to_vec() }
    }

    fn make_upgrade_add(name: &str) -> TreeNameUpgrade {
        TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: false }
    }

    fn make_upgrade_delete(name: &str) -> TreeNameUpgrade {
        TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: true }
    }

    /// Helper: open a fresh DB with default config and a "bank" tree.
    fn open_fresh_db(dir: &Path) -> DB {
        let config = default_config();
        let mut db = DB::open(dir, 0, &config, false).unwrap();
        db.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();
        db
    }

    #[test]
    fn test_db_open_close() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let config = default_config();
        let mut db = DB::open(&dir, 0, &config, false).unwrap();
        assert_eq!(db.version(), 0);
        db.close().unwrap();
    }

    #[test]
    fn test_db_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let mut db = open_fresh_db(&dir);

        // Apply changesets and commit
        let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"balance", b"100")])];
        db.apply_change_sets(&cs).unwrap();

        let v = db.commit().unwrap();
        assert_eq!(v, 1);
        assert_eq!(db.version(), 1);

        // Verify data
        let tree = db.tree_by_name("bank").unwrap();
        assert_eq!(tree.get(b"balance"), Some(b"100".to_vec()));

        // Second commit
        let cs2 = vec![make_named_changeset("bank", vec![make_kv_pair(b"balance", b"200")])];
        db.apply_change_sets(&cs2).unwrap();
        let v2 = db.commit().unwrap();
        assert_eq!(v2, 2);

        db.close().unwrap();
    }

    #[test]
    fn test_db_wal_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        // Open, add tree, commit 5 versions
        {
            let mut db = open_fresh_db(&dir);
            for i in 1..=5 {
                let cs = vec![make_named_changeset(
                    "bank",
                    vec![make_kv_pair(format!("key{i}").as_bytes(), format!("val{i}").as_bytes())],
                )];
                db.apply_change_sets(&cs).unwrap();
                let v = db.commit().unwrap();
                assert_eq!(v, i as i64);
            }
            db.close().unwrap();
        }

        // Reopen -- WAL should replay and catch up
        {
            let config = default_config();
            let db = DB::open(&dir, 0, &config, false).unwrap();
            assert_eq!(db.version(), 5);

            // Verify data from all 5 commits
            let tree = db.tree_by_name("bank").unwrap();
            for i in 1..=5 {
                assert_eq!(
                    tree.get(format!("key{i}").as_bytes()),
                    Some(format!("val{i}").into_bytes()),
                    "key{i} should exist after WAL replay"
                );
            }
        }
    }

    #[test]
    fn test_db_snapshot_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        // Open, commit a few versions, then rewrite snapshot
        let mut db = open_fresh_db(&dir);
        for i in 1..=3 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            db.apply_change_sets(&cs).unwrap();
            db.commit().unwrap();
        }
        assert_eq!(db.version(), 3);

        // Rewrite snapshot
        db.rewrite_snapshot().unwrap();
        db.close().unwrap();

        // Reopen -- should load from the new snapshot at version 3
        let config = default_config();
        let db2 = DB::open(&dir, 0, &config, false).unwrap();
        assert_eq!(db2.version(), 3);

        let tree = db2.tree_by_name("bank").unwrap();
        assert_eq!(tree.get(b"k1"), Some(b"v1".to_vec()));
        assert_eq!(tree.get(b"k2"), Some(b"v2".to_vec()));
        assert_eq!(tree.get(b"k3"), Some(b"v3".to_vec()));
    }

    #[test]
    fn test_db_snapshot_trigger() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let config = MemIavlConfig {
            async_commit_buffer: 0,
            snapshot_keep_recent: 2,
            snapshot_interval: 5,
            snapshot_min_time_interval: 0, // no time limit
            ..Default::default()
        };

        let mut db = DB::open(&dir, 0, &config, false).unwrap();
        db.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        // Commit 5 versions -- snapshot should be triggered at version 5 (background)
        for i in 1..=5 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            db.apply_change_sets(&cs).unwrap();
            db.commit().unwrap();
        }

        // The background snapshot was triggered at version 5 but may not have
        // completed yet. Commit more versions to let check_background_snapshot_rewrite
        // pick up the result.
        let cs6 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k6", b"v6")])];
        db.apply_change_sets(&cs6).unwrap();
        db.commit().unwrap();

        // Give the background thread a moment to finish, then commit again
        // to ensure the result is processed.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let cs7 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k7", b"v7")])];
        db.apply_change_sets(&cs7).unwrap();
        db.commit().unwrap();

        // Verify a snapshot directory exists for version 5
        let snap5 = dir.join(snapshot_name(5));
        assert!(snap5.exists(), "snapshot at version 5 should exist after interval trigger");

        db.close().unwrap();
    }

    #[test]
    fn test_db_rollback_to_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        // Open, commit 10 versions, snapshot at version 5
        {
            let mut db = open_fresh_db(&dir);
            for i in 1..=10 {
                let cs = vec![make_named_changeset(
                    "bank",
                    vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
                )];
                db.apply_change_sets(&cs).unwrap();
                db.commit().unwrap();

                if i == 5 {
                    db.rewrite_snapshot().unwrap();
                }
            }
            db.close().unwrap();
        }

        // Reopen targeting version 5 -- should load snapshot at 5
        {
            let config = default_config();
            let db = DB::open(&dir, 5, &config, false).unwrap();
            assert_eq!(db.version(), 5);

            let tree = db.tree_by_name("bank").unwrap();
            assert_eq!(tree.get(b"k5"), Some(b"v5".to_vec()));
            // Keys from versions 6-10 should NOT be present
            assert!(tree.get(b"k6").is_none(), "k6 should not exist at version 5");
        }
    }

    #[test]
    fn test_db_copy_readonly() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let mut db = open_fresh_db(&dir);
        let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"key", b"val")])];
        db.apply_change_sets(&cs).unwrap();
        db.commit().unwrap();

        let copy = db.copy();
        assert!(copy.is_read_only());
        assert_eq!(copy.version(), 1);
        assert_eq!(copy.tree_by_name("bank").unwrap().get(b"key"), Some(b"val".to_vec()));

        // Copy should not have a WAL
        assert!(copy.get_wal().is_none());

        db.close().unwrap();
    }

    #[test]
    fn test_db_apply_upgrades() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let mut db = open_fresh_db(&dir);

        // Add another tree
        db.apply_upgrades(&[make_upgrade_add("staking")]).unwrap();
        assert!(db.tree_by_name("staking").is_some());

        // Apply changesets and commit
        let cs = vec![
            make_named_changeset("bank", vec![make_kv_pair(b"b1", b"v1")]),
            make_named_changeset("staking", vec![make_kv_pair(b"s1", b"v1")]),
        ];
        db.apply_change_sets(&cs).unwrap();
        db.commit().unwrap();

        assert_eq!(db.tree_by_name("bank").unwrap().get(b"b1"), Some(b"v1".to_vec()));
        assert_eq!(db.tree_by_name("staking").unwrap().get(b"s1"), Some(b"v1".to_vec()));

        // Delete a tree
        db.apply_upgrades(&[make_upgrade_delete("staking")]).unwrap();
        assert!(db.tree_by_name("staking").is_none());

        db.close().unwrap();
    }

    #[test]
    fn test_db_version_management() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let mut db = open_fresh_db(&dir);
        assert_eq!(db.version(), 0);
        assert_eq!(db.committed_version(), 0);

        let cs = vec![make_named_changeset("bank", vec![make_kv_pair(b"k", b"v")])];
        db.apply_change_sets(&cs).unwrap();
        let v = db.commit().unwrap();
        assert_eq!(v, 1);
        assert_eq!(db.version(), 1);
        assert_eq!(db.committed_version(), 1);

        // Working commit info should reflect the next version
        let wci = db.working_commit_info();
        assert_eq!(wci.version, 2);

        // Last commit info should reflect the current version
        let lci = db.last_commit_info();
        assert_eq!(lci.version, 1);

        db.close().unwrap();
    }

    #[test]
    fn test_db_exclusive_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let config = default_config();
        let _db1 = DB::open(&dir, 0, &config, false).unwrap();

        // Second open should fail due to exclusive lock
        let result = DB::open(&dir, 0, &config, false);
        assert!(result.is_err(), "second open should fail due to exclusive lock");
    }

    #[test]
    fn test_db_background_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let config = MemIavlConfig {
            async_commit_buffer: 0,
            snapshot_keep_recent: 2,
            snapshot_interval: 5,
            snapshot_min_time_interval: 0,
            ..Default::default()
        };

        let mut db = DB::open(&dir, 0, &config, false).unwrap();
        db.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        // Commit 5 versions -- background snapshot triggered at version 5
        for i in 1..=5 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("key{i}").as_bytes(), format!("val{i}").as_bytes())],
            )];
            db.apply_change_sets(&cs).unwrap();
            db.commit().unwrap();
        }

        // At this point a background snapshot should be in progress
        assert!(
            db.snapshot_result_rx.is_some(),
            "background snapshot should have been triggered at version 5"
        );

        // Commit a few more versions while background snapshot is in progress
        for i in 6..=8 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("key{i}").as_bytes(), format!("val{i}").as_bytes())],
            )];
            db.apply_change_sets(&cs).unwrap();
            db.commit().unwrap();
        }

        // Wait for background thread and commit again to process the result
        std::thread::sleep(std::time::Duration::from_millis(500));
        let cs9 = vec![make_named_changeset("bank", vec![make_kv_pair(b"key9", b"val9")])];
        db.apply_change_sets(&cs9).unwrap();
        db.commit().unwrap();

        // Background snapshot should have been processed (rx consumed)
        assert!(
            db.snapshot_result_rx.is_none(),
            "background snapshot result should have been consumed"
        );

        // Verify snapshot directory exists
        let snap5 = dir.join(snapshot_name(5));
        assert!(snap5.exists(), "snapshot at version 5 should exist");

        // Verify all data is intact after reload
        assert_eq!(db.version(), 9);
        let tree = db.tree_by_name("bank").unwrap();
        for i in 1..=9 {
            assert_eq!(
                tree.get(format!("key{i}").as_bytes()),
                Some(format!("val{i}").into_bytes()),
                "key{i} should exist after background snapshot reload"
            );
        }

        db.close().unwrap();

        // Reopen and verify data persists
        let db2 = DB::open(&dir, 0, &config, false).unwrap();
        assert_eq!(db2.version(), 9);
        let tree2 = db2.tree_by_name("bank").unwrap();
        for i in 1..=9 {
            assert_eq!(
                tree2.get(format!("key{i}").as_bytes()),
                Some(format!("val{i}").into_bytes()),
                "key{i} should persist after reopen"
            );
        }
    }

    #[test]
    fn test_db_background_snapshot_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdb");

        let mut db = open_fresh_db(&dir);

        // Commit 3 versions
        for i in 1..=3 {
            let cs = vec![make_named_changeset(
                "bank",
                vec![make_kv_pair(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())],
            )];
            db.apply_change_sets(&cs).unwrap();
            db.commit().unwrap();
        }

        // Explicitly trigger background snapshot
        db.rewrite_snapshot_background().unwrap();
        assert!(db.snapshot_result_rx.is_some());

        // Trying to trigger again should fail
        let result = db.rewrite_snapshot_background();
        assert!(result.is_err(), "should not allow two concurrent background snapshots");

        // Wait for completion and commit to process
        std::thread::sleep(std::time::Duration::from_millis(500));

        let cs4 = vec![make_named_changeset("bank", vec![make_kv_pair(b"k4", b"v4")])];
        db.apply_change_sets(&cs4).unwrap();
        db.commit().unwrap();

        // Snapshot at version 3 should exist
        let snap3 = dir.join(snapshot_name(3));
        assert!(snap3.exists(), "snapshot at version 3 should exist");

        // Data should be intact
        assert_eq!(db.version(), 4);
        let tree = db.tree_by_name("bank").unwrap();
        for i in 1..=4 {
            assert_eq!(tree.get(format!("k{i}").as_bytes()), Some(format!("v{i}").into_bytes()),);
        }

        db.close().unwrap();
    }
}
