use alloy_primitives::B256;
use alloy_trie::Nibbles;
use memmap2::{Mmap, MmapOptions};
use mptdb_common::error::{MptDbError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use super::{
    flat_layout::{
        append_page as append_flat_page, data_path as pages_data_path,
        index_path as pages_index_path, open_data_file as open_pages_data_file,
        open_index_file as open_pages_index_file_handle, read_page_header, read_page_header_light,
        write_full_page_index, FlatPageIndexEntry, PAGE_INDEX_RECORD_LEN,
    },
    manifest::VersionManifest,
    segment::{
        MappedSegmentPage, SegmentPageLease, StoragePathTrace, StorageSegmentLocator,
        StorageTrieSegment, StorageTrieSegmentReader,
    },
    storage_cow::StorageTrieCow,
};

// pbldlt02: added total_len (u32) field to delta records so open_trie_page
// can construct MappedSegmentPage without read_page_header_light.
const DELTA_MAGIC: &[u8; 8] = b"pbldlt02";
const DELTA_RECORD_LEN: usize = 1 + 32 + 32 + 8 + 4 + 4 + 2;
const DELTA_OP_PUT: u8 = 1;
const DELTA_OP_DELETE: u8 = 2;
const REWRITE_INTERVAL: usize = 64;
pub(crate) const PUBLISHED_REWRITE_INTERVAL: usize = REWRITE_INTERVAL;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishedBaselineMeta {
    pub generation: u64,
    pub version: i64,
    pub root: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GenerationMeta {
    generation: u64,
    version: i64,
    root: B256,
    parent_generation: Option<u64>,
    depth: usize,
}

#[derive(Clone, Copy, Debug)]
struct DeltaEntry {
    page_off: u64,
    record_off: u32,
    /// Total byte length of the page in the data file.  Stored in the delta
    /// index so open_trie_page can construct MappedSegmentPage directly
    /// without calling read_page_header_light (saves one mmap header scan
    /// per L3 open on the hot path).
    total_len: u32,
    root: B256,
    format_version: u16,
}

/// Parsed representation of a delta file.
///
/// Records are loaded into HashMaps at open time so that lookup is O(1)
/// instead of a linear scan over the mmap.  For a 1M-record delta
/// (79 MB on disk), this trades ~52 MB of RAM for turning every
/// per-block `open_trie` call from O(N) to O(1).
#[derive(Clone)]
struct PublishedDeltaMmap {
    puts: Arc<HashMap<B256, DeltaEntry>>,
    deletes: Arc<HashSet<B256>>,
}

impl PublishedDeltaMmap {
    fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).map_err(|e| MptDbError::Other(format!("open delta file: {e}")))?;
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .map_err(|e| MptDbError::Other(format!("mmap delta file: {e}")))?
        };
        if mmap.len() < DELTA_MAGIC.len() || &mmap[..DELTA_MAGIC.len()] != DELTA_MAGIC {
            return Err(MptDbError::Other("invalid published delta header".to_string()));
        }

        // Parse all records into HashMaps for O(1) lookup.
        let estimated = (mmap.len() - DELTA_MAGIC.len()) / DELTA_RECORD_LEN;
        let mut puts = HashMap::with_capacity(estimated);
        let mut deletes = HashSet::new();
        let mut pos = DELTA_MAGIC.len();
        while pos + DELTA_RECORD_LEN <= mmap.len() {
            let op = mmap[pos];
            pos += 1;
            let key = B256::from_slice(&mmap[pos..pos + 32]);
            pos += 32;
            let root = B256::from_slice(&mmap[pos..pos + 32]);
            pos += 32;
            let page_off = u64::from_le_bytes(mmap[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let record_off = u32::from_le_bytes(mmap[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let total_len = u32::from_le_bytes(mmap[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let format_version = u16::from_le_bytes(mmap[pos..pos + 2].try_into().unwrap());
            pos += 2;

            match op {
                DELTA_OP_PUT => {
                    puts.insert(
                        key,
                        DeltaEntry { page_off, record_off, total_len, root, format_version },
                    );
                }
                DELTA_OP_DELETE => {
                    deletes.insert(key);
                }
                _ => {}
            }
        }
        // mmap is dropped here — all data is in the HashMaps.
        Ok(Self { puts: Arc::new(puts), deletes: Arc::new(deletes) })
    }
}

#[derive(Clone)]
pub(crate) struct PublishedGenerationResult {
    pub meta: PublishedBaselineMeta,
}

pub struct PublishedBaselineReader {
    meta: PublishedBaselineMeta,
    deltas: Vec<PublishedDeltaMmap>,
    /// Merged latest PUT index across the delta chain (newest wins).
    merged_puts: HashMap<B256, DeltaEntry>,
    /// Merged latest DELETE tombstones across the delta chain (newest wins).
    merged_deletes: HashSet<B256>,
    data_mmap: Arc<Mmap>,
    /// Stable identifier of pages.data when this reader was opened.  Used by
    /// `try_extend_published_store` to detect if the file was replaced by
    /// compaction (atomic rename), which invalidates old locators.
    data_file_id: u128,
    leases: Arc<Mutex<HashMap<u64, usize>>>,
    pinned_records: Mutex<HashSet<(u64, u32)>>,
    record_pins: Arc<Mutex<HashMap<(u64, u32), usize>>>,
}

/// Returns a platform-specific file identifier that changes when a file is
/// replaced via atomic rename.
fn data_file_id(file: &File) -> u128 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file.metadata().map(|m| m.ino() as u128).unwrap_or(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Keep full file index plus volume serial to avoid truncation collisions.
        file.metadata()
            .map(|m| {
                let file_index = m.file_index().unwrap_or(0) as u128;
                let volume_serial = m.volume_serial_number().map(u128::from).unwrap_or(0);
                (volume_serial << 64) | file_index
            })
            .unwrap_or(0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

pub struct PublishedTrieMaterialized {
    pub trace: StoragePathTrace,
    pub lookup_elapsed: Duration,
    pub materialize_elapsed: Duration,
}

pub struct PublishedTriePageLoaded {
    pub lease: Arc<SegmentPageLease>,
    pub lookup_elapsed: Duration,
}

pub struct PublishedTrieLoaded {
    pub trie: StorageTrieCow,
    pub lookup_elapsed: Duration,
}

impl PublishedBaselineReader {
    pub fn meta(&self) -> &PublishedBaselineMeta {
        &self.meta
    }

    pub fn materialize_touched_paths(
        &self,
        hashed_address: &B256,
        expected_root: B256,
        keys: &[Nibbles],
    ) -> Result<Option<PublishedTrieMaterialized>> {
        let loaded = match self.open_trie_page(hashed_address, expected_root)? {
            Some(loaded) => loaded,
            None => return Ok(None),
        };
        let reader = StorageTrieSegmentReader::open_shared_page(
            &loaded.lease,
            expected_root,
            loaded.lease.root_record_off(),
        )?;
        let materialize_start = std::time::Instant::now();
        let trace = reader.cursor().trace_paths(keys)?;
        let materialize_elapsed = materialize_start.elapsed();
        Ok(Some(PublishedTrieMaterialized {
            trace,
            lookup_elapsed: loaded.lookup_elapsed,
            materialize_elapsed,
        }))
    }

    pub fn open_trie_page(
        &self,
        hashed_address: &B256,
        expected_root: B256,
    ) -> Result<Option<PublishedTriePageLoaded>> {
        let lookup_start = std::time::Instant::now();
        let entry = match self.lookup_entry(hashed_address)? {
            Some(entry) => entry,
            None => return Ok(None),
        };
        if entry.root != expected_root {
            return Ok(None);
        }
        let mmap = Arc::clone(&self.data_mmap);
        let start = entry.page_off as usize;
        let total_len = entry.total_len as usize;
        // Phase 2: total_len is stored in the delta index — skip read_page_header entirely.
        // Only verify bounds; root and record_off were validated at publish time.
        let end = start.saturating_add(total_len);
        if start >= mmap.len() || end > mmap.len() {
            return Ok(None);
        }
        let mapped = Arc::new(MappedSegmentPage::new(mmap, start, total_len));
        let lease = Arc::new(SegmentPageLease::new(mapped, expected_root, entry.record_off));
        self.pin_record((entry.page_off, entry.record_off));
        Ok(Some(PublishedTriePageLoaded { lease, lookup_elapsed: lookup_start.elapsed() }))
    }

    pub fn open_trie(
        &self,
        hashed_address: &B256,
        expected_root: B256,
    ) -> Result<Option<PublishedTrieLoaded>> {
        let loaded = match self.open_trie_page(hashed_address, expected_root)? {
            Some(loaded) => loaded,
            None => return Ok(None),
        };
        Ok(Some(PublishedTrieLoaded {
            trie: StorageTrieCow::from_segment_page(loaded.lease),
            lookup_elapsed: loaded.lookup_elapsed,
        }))
    }

    fn lookup_entry(&self, hashed_address: &B256) -> Result<Option<DeltaEntry>> {
        if let Some(entry) = self.merged_puts.get(hashed_address) {
            return Ok(Some(*entry));
        }
        if self.merged_deletes.contains(hashed_address) {
            return Ok(None);
        }
        Ok(None)
    }

    fn pin_record(&self, record: (u64, u32)) {
        let mut local = self.pinned_records.lock();
        if !local.insert(record) {
            return;
        }
        drop(local);
        let mut global = self.record_pins.lock();
        *global.entry(record).or_insert(0) += 1;
    }
}

impl Drop for PublishedBaselineReader {
    fn drop(&mut self) {
        {
            let mut leases = self.leases.lock();
            match leases.get_mut(&self.meta.generation) {
                Some(count) if *count > 1 => *count -= 1,
                Some(_) => {
                    leases.remove(&self.meta.generation);
                }
                None => {}
            }
        }
        {
            let mut record_pins = self.record_pins.lock();
            for record in self.pinned_records.lock().iter() {
                match record_pins.get_mut(record) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(_) => {
                        record_pins.remove(record);
                    }
                    None => {}
                }
            }
        }
    }
}

/// Simple token-bucket IO rate limiter for background snapshot writes.
///
/// Mirrors sei-db's `GlobalRateLimiter` shared across all tree writes during
/// background snapshot rewrite, preventing the background worker from
/// saturating disk bandwidth and impacting frontend commit latency.
pub(crate) struct IoRateLimiter {
    bytes_per_sec: u64,
    written: u64,
    start: std::time::Instant,
}

impl IoRateLimiter {
    pub(crate) fn new(mb_per_sec: u64) -> Option<Self> {
        if mb_per_sec == 0 {
            None
        } else {
            Some(Self {
                bytes_per_sec: mb_per_sec * 1024 * 1024,
                written: 0,
                start: std::time::Instant::now(),
            })
        }
    }

    /// Account for `n` bytes written and sleep if we're ahead of the rate.
    pub(crate) fn account(&mut self, n: usize) {
        self.written += n as u64;
        let elapsed = self.start.elapsed();
        let allowed = (elapsed.as_secs_f64() * self.bytes_per_sec as f64) as u64;
        if self.written > allowed {
            let excess = self.written - allowed;
            let sleep_secs = excess as f64 / self.bytes_per_sec as f64;
            std::thread::sleep(std::time::Duration::from_secs_f64(sleep_secs));
        }
    }
}

/// Streaming segment writer for bulk_load — matching sei-db's snapshotWriter.
///
/// Keeps `pages.data` and `pages.index` open with buffered I/O across chunk
/// commits.  Delta records accumulate in memory (~79 bytes each).  At finalize,
/// ONE delta file + generation metadata is written and activated.
///
/// This eliminates the per-chunk `publish_generation` overhead (file creation,
/// atomic rename, generation metadata) and the post-hoc readback from RocksDB
/// that `build_full_published_segments` required.
pub(crate) struct BulkSegmentWriter {
    base_dir: PathBuf,
    data_file: std::io::BufWriter<File>,
    pages_index_file: std::io::BufWriter<File>,
    next_page_off: u64,
    /// Accumulated delta records: (op, hashed_address, root, page_off, record_off, total_len,
    /// format_version)
    delta_records: Vec<(u8, B256, B256, u64, u32, u32, u16)>,
    segments_written: usize,
}

impl BulkSegmentWriter {
    pub(crate) fn new(base_dir: &Path) -> Result<Self> {
        // Ensure directories exist.
        fs::create_dir_all(PublishedBaselineManager::published_dir(base_dir))
            .map_err(|e| MptDbError::Other(format!("create published dir for bulk: {e}")))?;
        fs::create_dir_all(PublishedBaselineManager::meta_dir(base_dir))
            .map_err(|e| MptDbError::Other(format!("create meta dir for bulk: {e}")))?;

        let mut data_file = PublishedBaselineManager::open_data_file(base_dir)?;
        let mut pages_index_file = PublishedBaselineManager::open_pages_index_file(base_dir)?;

        let next_page_off = data_file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek pages data for bulk: {e}")))?;
        pages_index_file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek pages index for bulk: {e}")))?;

        Ok(Self {
            base_dir: base_dir.to_path_buf(),
            data_file: std::io::BufWriter::with_capacity(128 * 1024, data_file),
            pages_index_file: std::io::BufWriter::with_capacity(64 * 1024, pages_index_file),
            next_page_off,
            delta_records: Vec::new(),
            segments_written: 0,
        })
    }

    /// Append segments from one bulk_load chunk (called per chunk).
    ///
    /// Writes page data + index entries to buffered files.  Accumulates
    /// delta records in memory for the final delta file.
    pub(crate) fn append_segments(&mut self, puts: &[(B256, StorageTrieSegment)]) -> Result<()> {
        self.delta_records.reserve(puts.len());
        for (hashed_address, image) in puts {
            let entry = PublishedBaselineManager::append_page_streaming(
                self.data_file.get_mut(),
                self.pages_index_file.get_mut(),
                &mut self.next_page_off,
                image,
            )?;
            self.delta_records.push((
                DELTA_OP_PUT,
                *hashed_address,
                image.root(),
                entry.page_off,
                entry.root_record_off,
                entry.total_len,
                entry.layout_version,
            ));
            self.segments_written += 1;
        }
        Ok(())
    }

    /// Finalize: flush buffers, write ONE delta file + generation metadata,
    /// activate the published meta.  Returns the meta for `open_published_store`.
    pub(crate) fn finalize(
        mut self,
        version: i64,
        state_root: B256,
    ) -> Result<Option<PublishedBaselineMeta>> {
        if self.delta_records.is_empty() {
            return Ok(None);
        }

        // Flush buffered writes.
        self.data_file
            .flush()
            .map_err(|e| MptDbError::Other(format!("flush bulk pages data: {e}")))?;
        self.pages_index_file
            .flush()
            .map_err(|e| MptDbError::Other(format!("flush bulk pages index: {e}")))?;

        let generation = version as u64;

        // ONE delta file with all records (no parent, depth=0 — clean baseline).
        let mgr = PublishedBaselineManager::open(&self.base_dir)?;
        PublishedBaselineManager::write_delta_file(
            &mgr.delta_path(generation),
            &self.delta_records,
        )?;
        mgr.save_generation_meta(&GenerationMeta {
            generation,
            version,
            root: state_root,
            parent_generation: None,
            depth: 0,
        })?;
        let meta = PublishedBaselineMeta { generation, version, root: state_root };
        mgr.save_meta(&meta)?;
        Ok(Some(meta))
    }
}

pub struct PublishedBaselineManager {
    base_dir: PathBuf,
    leases: Arc<Mutex<HashMap<u64, usize>>>,
    record_pins: Arc<Mutex<HashMap<(u64, u32), usize>>>,
}

impl PublishedBaselineManager {
    pub fn open(base_dir: &Path) -> Result<Self> {
        fs::create_dir_all(Self::published_dir(base_dir))
            .map_err(|e| MptDbError::Other(format!("create published baseline dir: {e}")))?;
        fs::create_dir_all(Self::meta_dir(base_dir))
            .map_err(|e| MptDbError::Other(format!("create published baseline meta dir: {e}")))?;
        let _ = Self::open_data_file(base_dir)?;
        Ok(Self {
            base_dir: base_dir.to_path_buf(),
            leases: Arc::new(Mutex::new(HashMap::new())),
            record_pins: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn load_meta(&self) -> Result<Option<PublishedBaselineMeta>> {
        let path = self.meta_path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            fs::read(&path).map_err(|e| MptDbError::Other(format!("read baseline meta: {e}")))?;
        let meta = serde_json::from_slice(&bytes)
            .map_err(|e| MptDbError::Other(format!("parse baseline meta: {e}")))?;
        Ok(Some(meta))
    }

    pub(crate) fn meta_for_version(
        &self,
        version: i64,
        root: B256,
    ) -> Result<Option<PublishedBaselineMeta>> {
        let generation = version as u64;
        let meta = match self.load_generation_meta(generation)? {
            Some(meta) if meta.version == version && meta.root == root => meta,
            _ => return Ok(None),
        };
        Ok(Some(PublishedBaselineMeta {
            generation: meta.generation,
            version: meta.version,
            root: meta.root,
        }))
    }

    pub(crate) fn earliest_snapshot_version(&self) -> Result<Option<i64>> {
        let mut earliest: Option<i64> = None;
        for generation in self.list_generations()? {
            let Some(meta) = self.load_generation_meta(generation)? else {
                continue;
            };
            earliest = Some(match earliest {
                Some(cur) => cur.min(meta.version),
                None => meta.version,
            });
        }
        Ok(earliest)
    }

    pub fn open_published_store(
        &self,
        meta: &PublishedBaselineMeta,
    ) -> Result<Option<PublishedBaselineReader>> {
        let gen_meta = match self.load_generation_meta(meta.generation)? {
            Some(gen_meta) if gen_meta.version == meta.version && gen_meta.root == meta.root => {
                gen_meta
            }
            _ => return Ok(None),
        };
        let deltas = self.load_delta_chain(&gen_meta)?;
        let (merged_puts, merged_deletes) = Self::build_merged_lookup(&deltas);
        let data_file = Self::open_data_file(&self.base_dir)?;
        let file_id = data_file_id(&data_file);
        let data_mmap = unsafe {
            MmapOptions::new()
                .map(&data_file)
                .map_err(|e| MptDbError::Other(format!("mmap published baseline data: {e}")))?
        };
        self.acquire_generation_lease(meta.generation);
        Ok(Some(PublishedBaselineReader {
            meta: meta.clone(),
            deltas,
            merged_puts,
            merged_deletes,
            data_mmap: Arc::new(data_mmap),
            data_file_id: file_id,
            leases: Arc::clone(&self.leases),
            pinned_records: Mutex::new(HashSet::new()),
            record_pins: Arc::clone(&self.record_pins),
        }))
    }

    /// Incrementally extend an existing reader with one new generation delta.
    ///
    /// When the new generation's parent is the existing reader's generation,
    /// only the single new delta file needs to be parsed (O(new_entries)).
    /// This avoids re-parsing the full delta chain (O(all_historical_entries))
    /// on every block in wal_first mode.
    ///
    /// Returns None if the incremental extend is not possible (parent mismatch),
    /// in which case the caller should fall back to `open_published_store`.
    pub fn try_extend_published_store(
        &self,
        meta: &PublishedBaselineMeta,
        mut existing: PublishedBaselineReader,
    ) -> Result<Option<PublishedBaselineReader>> {
        let gen_meta = match self.load_generation_meta(meta.generation)? {
            Some(gen_meta) if gen_meta.version == meta.version && gen_meta.root == meta.root => {
                gen_meta
            }
            _ => return Ok(None),
        };
        // Walk the parent chain from the new generation back to the existing reader's
        // generation, collecting intermediate generations in reverse (newest first).
        // This handles the case where the background worker is multiple versions behind
        // the main thread, preventing fallback to full rebuild on every view refresh.
        const MAX_FAST_FORWARD: usize = 32;
        let mut chain: Vec<GenerationMeta> = Vec::new();
        chain.push(gen_meta);
        loop {
            let tip = chain.last().unwrap();
            if tip.parent_generation == Some(existing.meta.generation) {
                // Found the link — chain is complete.
                break;
            }
            if tip.parent_generation.is_none() || chain.len() >= MAX_FAST_FORWARD {
                // Chain doesn't reach the existing reader within the depth limit.
                return Ok(None);
            }
            let parent_gen = tip.parent_generation.unwrap();
            match self.load_generation_meta(parent_gen)? {
                Some(parent_meta) => chain.push(parent_meta),
                None => return Ok(None),
            }
        }
        // chain is ordered newest→oldest; reverse to get oldest→newest for apply order.
        chain.reverse();

        // Load deltas for each intermediate generation (now oldest→newest).
        let mut new_deltas: Vec<PublishedDeltaMmap> = Vec::with_capacity(chain.len());
        for step in &chain {
            let delta = PublishedDeltaMmap::open(&self.delta_path(step.generation))?;
            new_deltas.push(delta);
        }

        // Build the new delta list: new deltas (oldest first) followed by existing.
        let mut deltas = Vec::with_capacity(existing.deltas.len() + new_deltas.len());
        // Prepend in newest-first order (new_deltas is oldest→newest, so reverse).
        for d in new_deltas.iter().rev() {
            deltas.push(d.clone());
        }
        deltas.extend(std::mem::take(&mut existing.deltas));

        // Apply each new delta onto the merged lookup in oldest→newest order.
        let mut merged_puts = std::mem::take(&mut existing.merged_puts);
        let mut merged_deletes = std::mem::take(&mut existing.merged_deletes);
        for delta in &new_deltas {
            Self::apply_delta_to_merged_lookup(&mut merged_puts, &mut merged_deletes, delta);
        }

        // Re-mmap the data file to see any new segment pages appended by the
        // persist worker since the last open.
        let data_file = Self::open_data_file(&self.base_dir)?;
        // Compact replaces pages.data via atomic rename → new file, new inode.
        // Compare the file id of the current file against the one stored in the
        // existing reader.  If they differ, the file was replaced and old delta
        // locators are invalid — fall back to full rebuild.
        let current_file_id = data_file_id(&data_file);
        if current_file_id != existing.data_file_id {
            return Ok(None);
        }
        let data_mmap = unsafe {
            MmapOptions::new()
                .map(&data_file)
                .map_err(|e| MptDbError::Other(format!("mmap published baseline data: {e}")))?
        };
        self.acquire_generation_lease(meta.generation);
        Ok(Some(PublishedBaselineReader {
            meta: meta.clone(),
            deltas,
            merged_puts,
            merged_deletes,
            data_mmap: Arc::new(data_mmap),
            data_file_id: current_file_id,
            leases: Arc::clone(&self.leases),
            pinned_records: Mutex::new(HashSet::new()),
            record_pins: Arc::clone(&self.record_pins),
        }))
    }

    pub(crate) fn publish_generation(
        &self,
        prev: Option<&PublishedBaselineMeta>,
        version: i64,
        root: B256,
        puts: &[(B256, StorageTrieSegment)],
        deletes: &[B256],
    ) -> Result<PublishedGenerationResult> {
        self.publish_generation_inner(prev, version, root, puts, deletes, true)
    }

    pub(crate) fn stage_generation(
        &self,
        prev: Option<&PublishedBaselineMeta>,
        version: i64,
        root: B256,
        puts: &[(B256, StorageTrieSegment)],
        deletes: &[B256],
    ) -> Result<PublishedGenerationResult> {
        self.publish_generation_inner(prev, version, root, puts, deletes, false)
    }

    fn publish_generation_inner(
        &self,
        prev: Option<&PublishedBaselineMeta>,
        version: i64,
        root: B256,
        puts: &[(B256, StorageTrieSegment)],
        deletes: &[B256],
        activate_meta: bool,
    ) -> Result<PublishedGenerationResult> {
        self.publish_generation_inner_with_limiter(
            prev,
            version,
            root,
            puts,
            deletes,
            activate_meta,
            None,
        )
    }

    /// Write a generation with rate limiting but do NOT activate the meta yet.
    /// The caller must explicitly call `activate_published_meta` after all
    /// post-processing (compact, WAL prune) succeeds.  This ensures the meta
    /// file is only updated atomically at the end.
    pub(crate) fn publish_generation_rate_limited(
        &self,
        prev: Option<&PublishedBaselineMeta>,
        version: i64,
        root: B256,
        puts: &[(B256, StorageTrieSegment)],
        deletes: &[B256],
        limiter: Option<&mut IoRateLimiter>,
    ) -> Result<PublishedGenerationResult> {
        self.publish_generation_inner_with_limiter(
            prev, version, root, puts, deletes, false, limiter,
        )
    }

    fn publish_generation_inner_with_limiter(
        &self,
        prev: Option<&PublishedBaselineMeta>,
        version: i64,
        root: B256,
        puts: &[(B256, StorageTrieSegment)],
        deletes: &[B256],
        activate_meta: bool,
        mut limiter: Option<&mut IoRateLimiter>,
    ) -> Result<PublishedGenerationResult> {
        let generation = version as u64;
        let prev_meta = match prev {
            Some(meta) => self.load_generation_meta(meta.generation)?,
            None => None,
        };

        let mut rewritten_index = None;
        let parent_generation = if let Some(prev_meta) = prev_meta.as_ref() {
            if prev_meta.depth + 1 >= REWRITE_INTERVAL {
                rewritten_index = Some(self.load_merged_index(prev_meta)?);
                None
            } else {
                Some(prev_meta.generation)
            }
        } else {
            None
        };

        let mut data_file = Self::open_data_file(&self.base_dir)?;
        let mut pages_index_file = Self::open_pages_index_file(&self.base_dir)?;
        let mut next_page_off = data_file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek published data append: {e}")))?;
        pages_index_file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek published pages index append: {e}")))?;
        let mut delta_records = Vec::new();
        if let Some(mut full_index) = rewritten_index {
            for hashed_address in deletes {
                full_index.remove(hashed_address);
            }
            for (hashed_address, image) in puts {
                if let Some(ref mut lim) = limiter {
                    lim.account(image.as_bytes().len());
                }
                let page = Self::append_page_streaming(
                    &mut data_file,
                    &mut pages_index_file,
                    &mut next_page_off,
                    image,
                )?;
                full_index.insert(
                    *hashed_address,
                    DeltaEntry {
                        page_off: page.page_off,
                        record_off: page.root_record_off,
                        total_len: page.total_len,
                        root: image.root(),
                        format_version: page.layout_version,
                    },
                );
            }
            delta_records.reserve(full_index.len());
            for (hashed_address, entry) in full_index {
                delta_records.push((
                    DELTA_OP_PUT,
                    hashed_address,
                    entry.root,
                    entry.page_off,
                    entry.record_off,
                    entry.total_len,
                    entry.format_version,
                ));
            }
        } else {
            delta_records.reserve(puts.len() + deletes.len());
            for hashed_address in deletes {
                delta_records.push((DELTA_OP_DELETE, *hashed_address, B256::ZERO, 0, 0, 0, 0));
            }
            for (hashed_address, image) in puts {
                if let Some(ref mut lim) = limiter {
                    lim.account(image.as_bytes().len());
                }
                let page = Self::append_page_streaming(
                    &mut data_file,
                    &mut pages_index_file,
                    &mut next_page_off,
                    image,
                )?;
                delta_records.push((
                    DELTA_OP_PUT,
                    *hashed_address,
                    image.root(),
                    page.page_off,
                    page.root_record_off,
                    page.total_len,
                    page.layout_version,
                ));
            }
        }

        Self::write_delta_file(&self.delta_path(generation), &delta_records)?;
        let generation_meta = GenerationMeta {
            generation,
            version,
            root,
            parent_generation,
            depth: parent_generation
                .and_then(|g| self.load_generation_meta(g).ok().flatten().map(|m| m.depth + 1))
                .unwrap_or(0),
        };
        self.save_generation_meta(&generation_meta)?;

        let meta = PublishedBaselineMeta { generation, version, root };
        if activate_meta {
            self.save_meta(&meta)?;
        }
        Ok(PublishedGenerationResult { meta })
    }

    pub fn activate_published_version(&self, version: i64, root: B256) -> Result<()> {
        let generation = version as u64;
        let meta = match self.load_generation_meta(generation)? {
            Some(meta) if meta.version == version && meta.root == root => meta,
            _ => {
                self.clear_meta()?;
                return Ok(());
            }
        };
        self.save_meta(&PublishedBaselineMeta {
            generation: meta.generation,
            version: meta.version,
            root: meta.root,
        })
    }

    pub(crate) fn activate_published_meta(&self, meta: &PublishedBaselineMeta) -> Result<()> {
        let stored = match self.load_generation_meta(meta.generation)? {
            Some(stored) if stored.version == meta.version && stored.root == meta.root => stored,
            _ => {
                self.clear_meta()?;
                return Ok(());
            }
        };
        self.save_meta(&PublishedBaselineMeta {
            generation: stored.generation,
            version: stored.version,
            root: stored.root,
        })
    }

    pub fn clear_meta(&self) -> Result<()> {
        let path = self.meta_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(MptDbError::Other(format!("remove baseline meta: {err}"))),
        }
    }

    pub fn compact_for_manifest(&self, manifest: &VersionManifest) -> Result<bool> {
        let mut keep_generations: HashSet<u64> =
            manifest.versions.keys().map(|v| *v as u64).collect();
        keep_generations.extend(self.active_generation_leases());
        keep_generations.extend(self.generations_with_pinned_records()?);

        let current_meta = self.load_meta()?;
        if let Some(meta) = current_meta.as_ref() {
            if !keep_generations.contains(&meta.generation) {
                self.clear_meta()?;
            }
        }

        let all_generations = self.list_generations()?;
        let kept_generations: Vec<u64> =
            all_generations.iter().copied().filter(|g| keep_generations.contains(g)).collect();

        let data_file = Self::open_data_file(&self.base_dir)?;
        let data_mmap = unsafe {
            MmapOptions::new()
                .map(&data_file)
                .map_err(|e| MptDbError::Other(format!("mmap compact source data: {e}")))?
        };

        let old_data_path = Self::data_path(&self.base_dir);
        let old_pages_index_path = Self::pages_index_path(&self.base_dir);
        let new_data_tmp = old_data_path.with_extension("data.tmp");
        let mut new_data = File::create(&new_data_tmp)
            .map_err(|e| MptDbError::Other(format!("create compacted data tmp: {e}")))?;
        let new_pages_index_tmp = old_pages_index_path.with_extension("index.tmp");
        let mut new_pages_index = File::create(&new_pages_index_tmp)
            .map_err(|e| MptDbError::Other(format!("create compacted pages index tmp: {e}")))?;
        new_data
            .write_all(super::flat_layout::SHARED_PAGES_DATA_MAGIC)
            .map_err(|e| MptDbError::Other(format!("write compacted data header: {e}")))?;
        new_pages_index
            .write_all(super::flat_layout::SHARED_PAGES_INDEX_MAGIC)
            .map_err(|e| MptDbError::Other(format!("write compacted pages index header: {e}")))?;

        let mut remap: HashMap<(u64, B256), (StorageSegmentLocator, u32)> = HashMap::new();
        let mut rewritten_pages = Vec::<FlatPageIndexEntry>::new();
        let mut rewrite_delta_records: Vec<(u64, Vec<(u8, B256, B256, u64, u32, u32, u16)>)> =
            Vec::new();
        let mut rewrite_generation_metas: Vec<GenerationMeta> = Vec::new();

        for generation in &kept_generations {
            let gen_meta = self.load_generation_meta(*generation)?.ok_or_else(|| {
                MptDbError::Other(format!("missing published generation metadata for {generation}"))
            })?;
            let merged = self.load_merged_index(&gen_meta)?;
            let mut rewritten = Vec::with_capacity(merged.len());
            for (key, entry) in merged {
                let (locator, total_len) = Self::remap_segment(
                    &mut new_data,
                    &mut new_pages_index,
                    &data_mmap,
                    &mut remap,
                    &mut rewritten_pages,
                    StorageSegmentLocator {
                        root: entry.root,
                        page_off: entry.page_off,
                        record_off: entry.record_off,
                        format_version: entry.format_version,
                    },
                )?;
                rewritten.push((
                    DELTA_OP_PUT,
                    key,
                    entry.root,
                    locator.page_off,
                    locator.record_off,
                    total_len,
                    locator.format_version,
                ));
            }
            rewrite_delta_records.push((*generation, rewritten));
            rewrite_generation_metas.push(GenerationMeta {
                generation: gen_meta.generation,
                version: gen_meta.version,
                root: gen_meta.root,
                parent_generation: None,
                depth: 0,
            });
        }

        new_data
            .flush()
            .map_err(|e| MptDbError::Other(format!("flush compacted data tmp: {e}")))?;
        write_full_page_index(&new_pages_index_tmp, &rewritten_pages)?;

        let mut delta_tmps = Vec::new();
        let mut meta_tmps = Vec::new();
        for (generation, records) in &rewrite_delta_records {
            let path = self.delta_path(*generation);
            let tmp = path.with_extension("delta.tmp");
            Self::write_delta_file(&tmp, records)?;
            delta_tmps.push((path, tmp));
        }
        for meta in &rewrite_generation_metas {
            let path = self.generation_meta_path(meta.generation);
            let tmp = path.with_extension("json.tmp");
            let bytes = serde_json::to_vec_pretty(meta).map_err(|e| {
                MptDbError::Other(format!("serialize compacted generation meta: {e}"))
            })?;
            fs::write(&tmp, bytes)
                .map_err(|e| MptDbError::Other(format!("write compacted generation meta: {e}")))?;
            meta_tmps.push((path, tmp));
        }

        fs::rename(&new_data_tmp, &old_data_path)
            .map_err(|e| MptDbError::Other(format!("rename compacted data file: {e}")))?;
        fs::rename(&new_pages_index_tmp, &old_pages_index_path)
            .map_err(|e| MptDbError::Other(format!("rename compacted pages index file: {e}")))?;
        for (path, tmp) in delta_tmps {
            fs::rename(&tmp, &path)
                .map_err(|e| MptDbError::Other(format!("rename compacted delta file: {e}")))?;
        }
        for (path, tmp) in meta_tmps {
            fs::rename(&tmp, &path)
                .map_err(|e| MptDbError::Other(format!("rename compacted generation meta: {e}")))?;
        }

        for generation in all_generations {
            if !keep_generations.contains(&generation) {
                let _ = fs::remove_file(self.delta_path(generation));
                let _ = fs::remove_file(self.generation_meta_path(generation));
            }
        }

        Ok(true)
    }

    pub fn active_generation_leases(&self) -> Vec<u64> {
        let mut out = self.leases.lock().keys().copied().collect::<Vec<_>>();
        out.sort_unstable();
        out
    }

    pub fn active_record_pins(&self) -> Vec<(u64, u32)> {
        let mut out = self.record_pins.lock().keys().copied().collect::<Vec<_>>();
        out.sort_unstable();
        out
    }

    fn generations_with_pinned_records(&self) -> Result<HashSet<u64>> {
        let pinned_records = self.record_pins.lock().keys().copied().collect::<HashSet<_>>();
        if pinned_records.is_empty() {
            return Ok(HashSet::new());
        }

        let mut out = HashSet::new();
        for generation in self.list_generations()? {
            let gen_meta = match self.load_generation_meta(generation)? {
                Some(meta) => meta,
                None => continue,
            };
            let merged = self.load_merged_index(&gen_meta)?;
            if merged
                .values()
                .any(|entry| pinned_records.contains(&(entry.page_off, entry.record_off)))
            {
                out.insert(generation);
            }
        }
        Ok(out)
    }

    fn load_merged_index(&self, meta: &GenerationMeta) -> Result<HashMap<B256, DeltaEntry>> {
        let mut by_generation = Vec::new();
        let mut cursor = Some(meta.generation);
        while let Some(generation) = cursor {
            let gen_meta = self.load_generation_meta(generation)?.ok_or_else(|| {
                MptDbError::Other(format!("missing published generation metadata for {generation}"))
            })?;
            by_generation.push(gen_meta.clone());
            cursor = gen_meta.parent_generation;
        }

        let mut seen = HashSet::new();
        let mut merged = HashMap::new();
        for gen_meta in by_generation {
            for (op, key, root, page_off, record_off, total_len, format_version) in
                Self::read_delta_file(&self.delta_path(gen_meta.generation))?
            {
                if !seen.insert(key) {
                    continue;
                }
                match op {
                    DELTA_OP_PUT => {
                        merged.insert(
                            key,
                            DeltaEntry { page_off, record_off, total_len, root, format_version },
                        );
                    }
                    DELTA_OP_DELETE => {}
                    _ => {}
                }
            }
        }
        Ok(merged)
    }

    fn load_delta_chain(&self, meta: &GenerationMeta) -> Result<Vec<PublishedDeltaMmap>> {
        let mut deltas = Vec::new();
        let mut cursor = Some(meta.generation);
        while let Some(generation) = cursor {
            let gen_meta = self.load_generation_meta(generation)?.ok_or_else(|| {
                MptDbError::Other(format!("missing published generation metadata for {generation}"))
            })?;
            deltas.push(PublishedDeltaMmap::open(&self.delta_path(gen_meta.generation))?);
            cursor = gen_meta.parent_generation;
        }
        Ok(deltas)
    }

    fn build_merged_lookup(
        deltas: &[PublishedDeltaMmap],
    ) -> (HashMap<B256, DeltaEntry>, HashSet<B256>) {
        let mut merged_puts = HashMap::new();
        let mut merged_deletes = HashSet::new();
        let mut seen = HashSet::new();

        // Deltas are ordered newest -> oldest.
        for delta in deltas {
            // Match per-delta lookup semantics: PUT wins over DELETE in the same delta.
            for (key, entry) in delta.puts.iter() {
                if seen.insert(*key) {
                    merged_puts.insert(*key, *entry);
                }
            }
            for key in delta.deletes.iter() {
                if seen.insert(*key) {
                    merged_deletes.insert(*key);
                }
            }
        }

        (merged_puts, merged_deletes)
    }

    fn apply_delta_to_merged_lookup(
        merged_puts: &mut HashMap<B256, DeltaEntry>,
        merged_deletes: &mut HashSet<B256>,
        delta: &PublishedDeltaMmap,
    ) {
        // Newest delta overrides historical state.
        for key in delta.deletes.iter() {
            merged_puts.remove(key);
            merged_deletes.insert(*key);
        }
        // PUT wins over DELETE inside the same delta (matches previous lookup semantics).
        for (key, entry) in delta.puts.iter() {
            merged_deletes.remove(key);
            merged_puts.insert(*key, *entry);
        }
    }

    fn published_dir(base: &Path) -> PathBuf {
        base.join("published")
    }

    fn meta_dir(base: &Path) -> PathBuf {
        base.join("meta")
    }

    fn data_path(base: &Path) -> PathBuf {
        pages_data_path(base)
    }

    fn pages_index_path(base: &Path) -> PathBuf {
        pages_index_path(base)
    }

    fn meta_path(&self) -> PathBuf {
        Self::meta_dir(&self.base_dir).join("published.json")
    }

    fn delta_path(&self, generation: u64) -> PathBuf {
        Self::published_dir(&self.base_dir).join(format!("gen-{generation}.delta"))
    }

    fn generation_meta_path(&self, generation: u64) -> PathBuf {
        Self::published_dir(&self.base_dir).join(format!("gen-{generation}.json"))
    }

    fn open_data_file(base_dir: &Path) -> Result<File> {
        let path = Self::data_path(base_dir);
        open_pages_data_file(&path)
    }

    fn open_pages_index_file(base_dir: &Path) -> Result<File> {
        open_pages_index_file_handle(&Self::pages_index_path(base_dir))
    }

    fn append_page_streaming(
        data_file: &mut File,
        pages_index_file: &mut File,
        next_page_off: &mut u64,
        segment: &StorageTrieSegment,
    ) -> Result<FlatPageIndexEntry> {
        let page_bytes = segment.as_bytes();
        let root = segment.root();
        let root_record_off = segment.root_record_off();
        let header = read_page_header(page_bytes)?;
        if header.root != root || header.root_record_off != root_record_off {
            return Err(MptDbError::Other("flat page header mismatch".to_string()));
        }

        let entry = FlatPageIndexEntry {
            page_off: *next_page_off,
            total_len: header.total_len,
            root,
            root_record_off,
            layout_version: header.layout_version,
            feature_flags: header.feature_flags,
            checksum: header.checksum,
        };

        data_file
            .write_all(page_bytes)
            .map_err(|e| MptDbError::Other(format!("append flat page data: {e}")))?;
        *next_page_off += u64::from(header.total_len);

        let mut record = [0u8; PAGE_INDEX_RECORD_LEN];
        let mut pos = 0usize;
        record[pos..pos + 8].copy_from_slice(&entry.page_off.to_le_bytes());
        pos += 8;
        record[pos..pos + 4].copy_from_slice(&entry.total_len.to_le_bytes());
        pos += 4;
        record[pos..pos + 32].copy_from_slice(entry.root.as_slice());
        pos += 32;
        record[pos..pos + 4].copy_from_slice(&entry.root_record_off.to_le_bytes());
        pos += 4;
        record[pos..pos + 2].copy_from_slice(&entry.layout_version.to_le_bytes());
        pos += 2;
        record[pos..pos + 2].copy_from_slice(&entry.feature_flags.to_le_bytes());
        pos += 2;
        record[pos..pos + 4].copy_from_slice(&entry.checksum.to_le_bytes());
        pages_index_file
            .write_all(&record)
            .map_err(|e| MptDbError::Other(format!("append flat page index record: {e}")))?;

        Ok(entry)
    }

    fn write_delta_file(
        path: &Path,
        records: &[(u8, B256, B256, u64, u32, u32, u16)],
    ) -> Result<()> {
        let tmp = path.with_extension("delta.tmp");
        let mut file = File::create(&tmp)
            .map_err(|e| MptDbError::Other(format!("create published delta file: {e}")))?;
        let mut bytes = Vec::with_capacity(DELTA_MAGIC.len() + records.len() * DELTA_RECORD_LEN);
        bytes.extend_from_slice(DELTA_MAGIC);
        for (op, key, root, page_off, record_off, total_len, format_version) in records {
            let mut record = [0u8; DELTA_RECORD_LEN];
            let mut pos = 0usize;
            record[pos] = *op;
            pos += 1;
            record[pos..pos + 32].copy_from_slice(key.as_slice());
            pos += 32;
            record[pos..pos + 32].copy_from_slice(root.as_slice());
            pos += 32;
            record[pos..pos + 8].copy_from_slice(&page_off.to_le_bytes());
            pos += 8;
            record[pos..pos + 4].copy_from_slice(&record_off.to_le_bytes());
            pos += 4;
            record[pos..pos + 4].copy_from_slice(&total_len.to_le_bytes());
            pos += 4;
            record[pos..pos + 2].copy_from_slice(&format_version.to_le_bytes());
            bytes.extend_from_slice(&record);
        }
        file.write_all(&bytes)
            .map_err(|e| MptDbError::Other(format!("write published delta file: {e}")))?;
        file.flush().map_err(|e| MptDbError::Other(format!("flush published delta file: {e}")))?;
        fs::rename(&tmp, path)
            .map_err(|e| MptDbError::Other(format!("rename published delta file: {e}")))?;
        Ok(())
    }

    fn read_delta_file(path: &Path) -> Result<Vec<(u8, B256, B256, u64, u32, u32, u16)>> {
        let mut file =
            File::open(path).map_err(|e| MptDbError::Other(format!("open delta file: {e}")))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| MptDbError::Other(format!("read delta file: {e}")))?;
        if bytes.len() < DELTA_MAGIC.len() || &bytes[..DELTA_MAGIC.len()] != DELTA_MAGIC {
            return Err(MptDbError::Other("invalid published delta header".to_string()));
        }

        let mut out = Vec::new();
        let mut pos = DELTA_MAGIC.len();
        while pos + DELTA_RECORD_LEN <= bytes.len() {
            let op = bytes[pos];
            pos += 1;
            let key = B256::from_slice(&bytes[pos..pos + 32]);
            pos += 32;
            let root = B256::from_slice(&bytes[pos..pos + 32]);
            pos += 32;
            let mut page_off_bytes = [0u8; 8];
            page_off_bytes.copy_from_slice(&bytes[pos..pos + 8]);
            let page_off = u64::from_le_bytes(page_off_bytes);
            pos += 8;
            let mut record_off_bytes = [0u8; 4];
            record_off_bytes.copy_from_slice(&bytes[pos..pos + 4]);
            let record_off = u32::from_le_bytes(record_off_bytes);
            pos += 4;
            let total_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let mut format_bytes = [0u8; 2];
            format_bytes.copy_from_slice(&bytes[pos..pos + 2]);
            let format_version = u16::from_le_bytes(format_bytes);
            pos += 2;
            out.push((op, key, root, page_off, record_off, total_len, format_version));
        }
        Ok(out)
    }

    fn save_meta(&self, meta: &PublishedBaselineMeta) -> Result<()> {
        let path = self.meta_path();
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec(meta)
            .map_err(|e| MptDbError::Other(format!("serialize baseline meta: {e}")))?;
        fs::write(&tmp, bytes)
            .map_err(|e| MptDbError::Other(format!("write baseline meta: {e}")))?;
        fs::rename(&tmp, &path)
            .map_err(|e| MptDbError::Other(format!("rename baseline meta: {e}")))?;
        Ok(())
    }

    fn save_generation_meta(&self, meta: &GenerationMeta) -> Result<()> {
        let path = self.generation_meta_path(meta.generation);
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec(meta)
            .map_err(|e| MptDbError::Other(format!("serialize generation meta: {e}")))?;
        fs::write(&tmp, bytes)
            .map_err(|e| MptDbError::Other(format!("write generation meta: {e}")))?;
        fs::rename(&tmp, &path)
            .map_err(|e| MptDbError::Other(format!("rename generation meta: {e}")))?;
        Ok(())
    }

    fn load_generation_meta(&self, generation: u64) -> Result<Option<GenerationMeta>> {
        let path = self.generation_meta_path(generation);
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            fs::read(&path).map_err(|e| MptDbError::Other(format!("read generation meta: {e}")))?;
        let meta = serde_json::from_slice(&bytes)
            .map_err(|e| MptDbError::Other(format!("parse generation meta: {e}")))?;
        Ok(Some(meta))
    }

    fn list_generations(&self) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(Self::published_dir(&self.base_dir))
            .map_err(|e| MptDbError::Other(format!("read published dir: {e}")))?
        {
            let entry =
                entry.map_err(|e| MptDbError::Other(format!("read published dir entry: {e}")))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("gen-") {
                if let Some(num) = rest.strip_suffix(".json") {
                    if let Ok(generation) = num.parse::<u64>() {
                        out.push(generation);
                    }
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Returns `(locator, total_len)` so callers can store `total_len` in
    /// the delta record without an additional header read.
    fn remap_segment(
        new_data: &mut File,
        new_pages_index: &mut File,
        old_data: &[u8],
        remap: &mut HashMap<(u64, B256), (StorageSegmentLocator, u32)>,
        rewritten_pages: &mut Vec<FlatPageIndexEntry>,
        locator: StorageSegmentLocator,
    ) -> Result<(StorageSegmentLocator, u32)> {
        let key = (locator.page_off, locator.root);
        if let Some(existing) = remap.get(&key).copied() {
            return Ok(existing);
        }

        let start = locator.page_off as usize;
        let page_header = read_page_header(old_data.get(start..).ok_or_else(|| {
            MptDbError::Other(format!(
                "segment remap page out of bounds: page_off={}, data_len={}, root={}, record_off={}",
                locator.page_off,
                old_data.len(),
                locator.root,
                locator.record_off
            ))
        })?)?;
        if page_header.root_record_off != locator.record_off {
            return Err(MptDbError::Other("segment remap root record offset mismatch".to_string()));
        }
        let end = start.saturating_add(page_header.total_len as usize);
        let bytes = old_data
            .get(start..end)
            .ok_or_else(|| MptDbError::Other("segment remap slice out of bounds".to_string()))?;
        let entry =
            append_flat_page(new_data, new_pages_index, bytes, locator.root, locator.record_off)?;
        rewritten_pages.push(entry);
        let new_locator = StorageSegmentLocator {
            root: locator.root,
            page_off: entry.page_off,
            record_off: entry.root_record_off,
            format_version: entry.layout_version,
        };
        let result = (new_locator, entry.total_len);
        remap.insert(key, result);
        Ok(result)
    }

    fn acquire_generation_lease(&self, generation: u64) {
        let mut leases = self.leases.lock();
        *leases.entry(generation).or_insert(0) += 1;
    }

    #[cfg(test)]
    fn acquire_record_pins(&self, records: &[(u64, u32)]) {
        let mut pins = self.record_pins.lock();
        for record in records {
            *pins.entry(*record).or_insert(0) += 1;
        }
    }

    #[cfg(test)]
    fn collect_pinned_records(index: &HashMap<B256, DeltaEntry>) -> Vec<(u64, u32)> {
        let mut out =
            index.values().map(|entry| (entry.page_off, entry.record_off)).collect::<Vec<_>>();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_trie::Nibbles;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    use crate::mpt::tree::MptTree;

    fn make_tree(byte: u8) -> (MptTree, B256) {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(byte));
        tree.insert(&key, vec![byte]);
        let root = tree.root_hash();
        (tree, root)
    }

    #[test]
    fn publish_and_reload_generation() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();
        let (tree, root) = make_tree(1);
        let image = StorageTrieSegment::from_tree(&tree, root).unwrap();
        let result = mgr
            .publish_generation(None, 1, root, &[(B256::with_last_byte(0x11), image)], &[])
            .unwrap();
        assert_eq!(result.meta.version, 1);
        let loaded = mgr.load_meta().unwrap().unwrap();
        assert_eq!(loaded, result.meta);
        let store = mgr.open_published_store(&loaded).unwrap().unwrap();
        let key = Nibbles::unpack(B256::with_last_byte(1));
        let loaded =
            store.materialize_touched_paths(&B256::with_last_byte(0x11), root, &[key]).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().trace.into_tree().root_hash(), root);
    }

    #[test]
    fn delta_publish_overlays_parent_without_copying_directory() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();

        let (tree1, root1) = make_tree(1);
        let image1 = StorageTrieSegment::from_tree(&tree1, root1).unwrap();
        let meta1 = mgr
            .publish_generation(None, 1, root1, &[(B256::with_last_byte(0x11), image1)], &[])
            .unwrap()
            .meta;

        let (tree2, root2) = make_tree(2);
        let image2 = StorageTrieSegment::from_tree(&tree2, root2).unwrap();
        let meta2 = mgr
            .publish_generation(
                Some(&meta1),
                2,
                root2,
                &[(B256::with_last_byte(0x22), image2)],
                &[],
            )
            .unwrap()
            .meta;

        let store = mgr.open_published_store(&meta2).unwrap().unwrap();
        let key1 = Nibbles::unpack(B256::with_last_byte(1));
        let key2 = Nibbles::unpack(B256::with_last_byte(2));
        assert!(store
            .materialize_touched_paths(&B256::with_last_byte(0x11), root1, &[key1])
            .unwrap()
            .is_some());
        assert!(store
            .materialize_touched_paths(&B256::with_last_byte(0x22), root2, &[key2])
            .unwrap()
            .is_some());
    }

    #[test]
    fn activate_or_clear_meta() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();
        mgr.clear_meta().unwrap();
        assert!(mgr.load_meta().unwrap().is_none());
    }

    #[test]
    fn published_reader_tracks_generation_leases_and_record_pins() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();
        let (tree, root) = make_tree(1);
        let image = StorageTrieSegment::from_tree(&tree, root).unwrap();
        let meta = mgr
            .publish_generation(None, 1, root, &[(B256::with_last_byte(0x11), image)], &[])
            .unwrap()
            .meta;

        assert!(mgr.active_generation_leases().is_empty());
        assert!(mgr.active_record_pins().is_empty());

        {
            let reader = mgr.open_published_store(&meta).unwrap().unwrap();
            assert_eq!(reader.meta(), &meta);
            assert_eq!(mgr.active_generation_leases(), vec![1]);
            assert!(mgr.active_record_pins().is_empty());
            let key = Nibbles::unpack(B256::with_last_byte(1));
            let loaded = reader
                .materialize_touched_paths(&B256::with_last_byte(0x11), root, &[key])
                .unwrap();
            assert!(loaded.is_some());
            let pins = mgr.active_record_pins();
            assert_eq!(pins.len(), 1);
            assert_eq!(pins[0].0, 8);
        }

        assert!(mgr.active_generation_leases().is_empty());
        assert!(mgr.active_record_pins().is_empty());
    }

    #[test]
    fn compact_keeps_pinned_generation() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();

        let (tree1, root1) = make_tree(1);
        let image1 = StorageTrieSegment::from_tree(&tree1, root1).unwrap();
        let meta1 = mgr
            .publish_generation(None, 1, root1, &[(B256::with_last_byte(0x11), image1)], &[])
            .unwrap()
            .meta;

        let (tree2, root2) = make_tree(2);
        let image2 = StorageTrieSegment::from_tree(&tree2, root2).unwrap();
        let _meta2 = mgr
            .publish_generation(
                Some(&meta1),
                2,
                root2,
                &[(B256::with_last_byte(0x22), image2)],
                &[],
            )
            .unwrap()
            .meta;

        let mut versions = BTreeMap::new();
        versions.insert(0, alloy_trie::EMPTY_ROOT_HASH);
        versions.insert(2, root2);
        let manifest = VersionManifest { earliest_version: 2, latest_version: 2, versions };

        let reader = mgr.open_published_store(&meta1).unwrap().unwrap();
        assert_eq!(mgr.active_generation_leases(), vec![1]);
        mgr.compact_for_manifest(&manifest).unwrap();
        assert!(mgr.load_generation_meta(1).unwrap().is_some());
        drop(reader);

        mgr.compact_for_manifest(&manifest).unwrap();
        assert!(mgr.load_generation_meta(1).unwrap().is_none());
    }

    #[test]
    fn compact_keeps_generation_from_record_pin_without_generation_lease() {
        let dir = TempDir::new().unwrap();
        let mgr = PublishedBaselineManager::open(dir.path()).unwrap();

        let (tree1, root1) = make_tree(1);
        let image1 = StorageTrieSegment::from_tree(&tree1, root1).unwrap();
        let _meta1 = mgr
            .publish_generation(None, 1, root1, &[(B256::with_last_byte(0x11), image1)], &[])
            .unwrap()
            .meta;

        let (tree2, root2) = make_tree(2);
        let image2 = StorageTrieSegment::from_tree(&tree2, root2).unwrap();
        let _meta2 = mgr
            .publish_generation(None, 2, root2, &[(B256::with_last_byte(0x22), image2)], &[])
            .unwrap()
            .meta;

        let mut versions = BTreeMap::new();
        versions.insert(0, alloy_trie::EMPTY_ROOT_HASH);
        versions.insert(2, root2);
        let manifest = VersionManifest { earliest_version: 2, latest_version: 2, versions };

        let gen1 = mgr.load_generation_meta(1).unwrap().unwrap();
        let merged1 = mgr.load_merged_index(&gen1).unwrap();
        let pinned = PublishedBaselineManager::collect_pinned_records(&merged1);
        assert_eq!(mgr.active_generation_leases(), Vec::<u64>::new());
        assert!(mgr.active_record_pins().is_empty());

        mgr.acquire_record_pins(&pinned);
        mgr.compact_for_manifest(&manifest).unwrap();
        assert!(mgr.load_generation_meta(1).unwrap().is_some());

        mgr.record_pins.lock().clear();
        mgr.compact_for_manifest(&manifest).unwrap();
        assert!(mgr.load_generation_meta(1).unwrap().is_none());
    }
}
