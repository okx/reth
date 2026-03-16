// Allow dead_code: this module is used by higher-level WAL components (T2.2+).
#![allow(dead_code)]

// WalLog — bottom-layer segment storage for the WAL.
//
// Segment file format (each entry is append-only):
//   [data_len: u32 LE]      — payload length
//   [crc32:    u32 LE]      — CRC32 of data bytes
//   [data:     [u8; data_len]] — payload
//
// Segment file naming: `{start_index:020}` (20-digit zero-padded).

use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use mptdb_common::error::{MptDbError, Result};

const ENTRY_HEADER_SIZE: u64 = 8; // 4 bytes data_len + 4 bytes crc32
const DEFAULT_SEGMENT_SIZE: u64 = 20 * 1024 * 1024; // 20 MB
const META_FILE: &str = "META";

/// Options controlling WalLog behaviour.
#[derive(Clone, Default)]
pub(crate) struct WalLogOptions {
    /// When true, skip `fsync` after every write (faster but less durable).
    pub no_sync: bool,
    /// Maximum segment file size in bytes before rolling over. 0 means use the default (20 MB).
    pub segment_size: u64,
}

impl WalLogOptions {
    fn effective_segment_size(&self) -> u64 {
        if self.segment_size == 0 {
            DEFAULT_SEGMENT_SIZE
        } else {
            self.segment_size
        }
    }
}

/// Metadata about a single segment file.
#[derive(Clone, Debug)]
struct SegmentInfo {
    path: PathBuf,
    start_index: u64,
    entry_count: u64,
}

/// Append-only, segment-based write-ahead log.
pub(crate) struct WalLog {
    dir: PathBuf,
    segments: Vec<SegmentInfo>,
    first_index: u64, // 0 = empty
    last_index: u64,  // 0 = empty
    current_writer: Option<BufWriter<File>>,
    /// Tracks the logical size of the current segment (including buffered bytes).
    current_segment_size: u64,
    opts: WalLogOptions,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl WalLog {
    /// Open (or create) a WAL directory, recovering the last segment if needed.
    pub fn open(dir: impl Into<PathBuf>, opts: WalLogOptions) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;

        let mut segments = load_segments(&dir)?;

        // Validate & recover the last segment if it exists.
        if let Some(last_seg) = segments.last_mut() {
            let (valid_count, valid_offset) = validate_segment(&last_seg.path)?;
            // Get actual file size to decide if truncation is needed.
            let file_len = fs::metadata(&last_seg.path)?.len();
            if valid_offset < file_len {
                // Truncation needed — the last entry was partially written.
                tracing::warn!(
                    path = %last_seg.path.display(),
                    valid_offset,
                    file_len,
                    "recovering corrupted segment"
                );
                recover_corrupted_segment(&last_seg.path)?;
            }
            last_seg.entry_count = valid_count;
        }

        // Remove any segments that ended up with 0 entries (empty trailing segment).
        segments.retain(|s| s.entry_count > 0);

        let (first_index, last_index) = if segments.is_empty() {
            (0, 0)
        } else {
            let physical_first = segments.first().unwrap().start_index;
            let last_seg = segments.last().unwrap();
            let last = last_seg.start_index + last_seg.entry_count - 1;
            // Logical first may be ahead of physical first if truncate_front
            // was called within a segment (entries before first_index still on
            // disk but logically removed). Load persisted meta if available.
            let meta_first = load_meta(&dir).unwrap_or(0);
            let first = if meta_first > physical_first { meta_first } else { physical_first };
            (first, last)
        };

        // Open the current (last) segment for appending.
        let (current_writer, current_segment_size) = if let Some(last_seg) = segments.last() {
            let size = fs::metadata(&last_seg.path)?.len();
            (Some(open_segment_writer(&last_seg.path)?), size)
        } else {
            (None, 0)
        };

        Ok(Self {
            dir,
            segments,
            first_index,
            last_index,
            current_writer,
            current_segment_size,
            opts,
        })
    }

    /// Append a single entry. `index` must equal `last_index + 1` (or be the first entry).
    pub fn write(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.validate_write_index(index)?;
        self.maybe_rollover()?;
        self.write_entry(data)?;

        if self.first_index == 0 {
            self.first_index = index;
        }
        self.last_index = index;

        // Bump entry count on current segment.
        if let Some(seg) = self.segments.last_mut() {
            seg.entry_count += 1;
        }

        if !self.opts.no_sync {
            self.sync()?;
        }

        Ok(())
    }

    /// Write a batch of entries, issuing a single fsync at the end.
    pub fn write_batch(&mut self, entries: &[(u64, Vec<u8>)]) -> Result<()> {
        for (index, data) in entries {
            self.validate_write_index(*index)?;
            self.maybe_rollover()?;
            self.write_entry(data)?;

            if self.first_index == 0 {
                self.first_index = *index;
            }
            self.last_index = *index;

            if let Some(seg) = self.segments.last_mut() {
                seg.entry_count += 1;
            }
        }

        if !self.opts.no_sync {
            self.sync()?;
        }

        Ok(())
    }

    /// Read the entry at `index`.
    pub fn read(&mut self, index: u64) -> Result<Vec<u8>> {
        // Flush buffered writes so the data is visible via a separate file handle.
        if let Some(ref mut w) = self.current_writer {
            w.flush()?;
        }
        if self.first_index == 0 || index < self.first_index || index > self.last_index {
            return Err(MptDbError::NotFound(format!("wal entry index {index}")));
        }

        let seg_idx = self
            .find_segment_for_index(index)
            .ok_or_else(|| MptDbError::NotFound(format!("segment for index {index}")))?;
        let seg = &self.segments[seg_idx];

        let file = File::open(&seg.path)?;
        let mut reader = BufReader::new(file);

        // Skip (index - start_index) entries.
        let skip_count = index - seg.start_index;
        for _ in 0..skip_count {
            let len = read_u32_le(&mut reader)?;
            let _crc = read_u32_le(&mut reader)?;
            // Skip data bytes.
            let mut remaining = len as u64;
            while remaining > 0 {
                let to_skip = remaining.min(8192);
                let mut buf = vec![0u8; to_skip as usize];
                reader.read_exact(&mut buf)?;
                remaining -= to_skip;
            }
        }

        // Read the target entry.
        let data_len = read_u32_le(&mut reader)?;
        let stored_crc = read_u32_le(&mut reader)?;
        let mut data = vec![0u8; data_len as usize];
        reader.read_exact(&mut data)?;

        let computed_crc = crc32fast::hash(&data);
        if computed_crc != stored_crc {
            return Err(MptDbError::Other(format!(
                "CRC mismatch at index {index}: stored={stored_crc:#x}, computed={computed_crc:#x}"
            )));
        }

        Ok(data)
    }

    pub fn first_index(&self) -> u64 {
        self.first_index
    }

    pub fn last_index(&self) -> u64 {
        self.last_index
    }

    /// Remove all entries with index < `index`. The entry at `index` is preserved.
    pub fn truncate_front(&mut self, index: u64) -> Result<()> {
        // Flush buffered writes so segment files are complete on disk.
        if let Some(ref mut w) = self.current_writer {
            w.flush()?;
        }

        if self.first_index == 0 || index <= self.first_index {
            return Ok(());
        }

        if index > self.last_index {
            // Truncate everything.
            return self.clear();
        }

        // Find segments to remove entirely (all entries < index).
        let mut remove_count = 0;
        for seg in &self.segments {
            let seg_last = seg.start_index + seg.entry_count - 1;
            if seg_last < index {
                // Entire segment is before `index` — remove it.
                fs::remove_file(&seg.path)?;
                remove_count += 1;
            } else {
                break;
            }
        }

        self.segments.drain(..remove_count);
        self.first_index = if self.segments.is_empty() {
            0
        } else {
            // The new first_index is `index`, but entries before `index` within
            // the first remaining segment still exist on disk. We keep them but
            // logically set first_index to `index`.
            index
        };

        if self.segments.is_empty() {
            self.last_index = 0;
            self.current_writer = None;
        }

        // Persist the logical first_index so it survives close + reopen.
        save_meta(&self.dir, self.first_index)?;

        Ok(())
    }

    /// Remove all entries with index > `index`. The entry at `index` is preserved.
    pub fn truncate_back(&mut self, index: u64) -> Result<()> {
        if self.first_index == 0 || index >= self.last_index {
            return Ok(());
        }

        if index < self.first_index {
            return self.clear();
        }

        // Flush and drop current writer before modifying files.
        if let Some(ref mut w) = self.current_writer {
            w.flush()?;
        }
        self.current_writer = None;

        // Find the segment containing `index`.
        let seg_idx = self
            .find_segment_for_index(index)
            .ok_or_else(|| MptDbError::NotFound(format!("segment for index {index}")))?;

        // Remove all segments after seg_idx.
        for seg in self.segments.drain(seg_idx + 1..) {
            fs::remove_file(&seg.path)?;
        }

        // Truncate the target segment file to keep only entries up to `index`.
        let seg = &mut self.segments[seg_idx];
        let keep_count = index - seg.start_index + 1;

        // Compute byte offset of the first entry to discard.
        let truncate_offset = compute_byte_offset(&seg.path, keep_count)?;
        let file = OpenOptions::new().write(true).open(&seg.path)?;
        file.set_len(truncate_offset)?;
        file.sync_all()?;

        seg.entry_count = keep_count;
        self.last_index = index;
        self.current_segment_size = truncate_offset;

        // Reopen writer on the truncated segment.
        self.current_writer = Some(open_segment_writer(&seg.path)?);

        Ok(())
    }

    /// Flush the current writer to disk.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(ref mut w) = self.current_writer {
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Sync and close the current writer.
    pub fn close(&mut self) -> Result<()> {
        self.sync()?;
        self.current_writer = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers on WalLog
// ---------------------------------------------------------------------------

impl WalLog {
    fn validate_write_index(&self, index: u64) -> Result<()> {
        if index == 0 {
            return Err(MptDbError::Other("wal index must start from 1".into()));
        }
        if self.last_index > 0 && index != self.last_index + 1 {
            return Err(MptDbError::Other(format!(
                "non-monotonic wal write: expected {}, got {index}",
                self.last_index + 1
            )));
        }
        Ok(())
    }

    /// If the current segment exceeds the size limit, roll over to a new one.
    fn maybe_rollover(&mut self) -> Result<()> {
        let need_new_segment = if self.current_writer.is_some() {
            self.current_segment_size >= self.opts.effective_segment_size()
        } else {
            true // no writer yet
        };

        if need_new_segment {
            // Flush old writer.
            if let Some(ref mut w) = self.current_writer {
                w.flush()?;
                w.get_ref().sync_all()?;
            }

            let next_index = if self.last_index == 0 { 1 } else { self.last_index + 1 };
            let seg_path = self.dir.join(segment_filename(next_index));
            let file = File::create(&seg_path)?;
            self.current_writer = Some(BufWriter::new(file));
            self.current_segment_size = 0;
            self.segments.push(SegmentInfo {
                path: seg_path,
                start_index: next_index,
                entry_count: 0,
            });
        }

        Ok(())
    }

    /// Write a single entry (header + data) to the current writer.
    fn write_entry(&mut self, data: &[u8]) -> Result<()> {
        let w = self.current_writer.as_mut().expect("writer must exist after maybe_rollover");
        let data_len = data.len() as u32;
        let crc = crc32fast::hash(data);
        w.write_all(&data_len.to_le_bytes())?;
        w.write_all(&crc.to_le_bytes())?;
        w.write_all(data)?;
        self.current_segment_size += ENTRY_HEADER_SIZE + data.len() as u64;
        Ok(())
    }

    /// Binary search segments to find which one contains `index`.
    fn find_segment_for_index(&self, index: u64) -> Option<usize> {
        if self.segments.is_empty() {
            return None;
        }

        // Find the last segment whose start_index <= index.
        let pos = self.segments.partition_point(|s| s.start_index <= index);
        if pos == 0 {
            return None;
        }
        let candidate = pos - 1;
        let seg = &self.segments[candidate];
        if index < seg.start_index + seg.entry_count {
            Some(candidate)
        } else {
            None
        }
    }

    fn clear(&mut self) -> Result<()> {
        self.current_writer = None;
        self.current_segment_size = 0;
        for seg in self.segments.drain(..) {
            let _ = fs::remove_file(&seg.path);
        }
        self.first_index = 0;
        self.last_index = 0;
        save_meta(&self.dir, 0)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn segment_filename(start_index: u64) -> String {
    format!("{start_index:020}")
}

fn parse_segment_filename(name: &str) -> Option<u64> {
    if name.len() == 20 && name.chars().all(|c| c.is_ascii_digit()) {
        name.parse::<u64>().ok()
    } else {
        None
    }
}

/// Load persisted logical first_index from META file. Returns 0 if not found.
fn load_meta(dir: &Path) -> Result<u64> {
    let path = dir.join(META_FILE);
    match fs::read(&path) {
        Ok(data) if data.len() == 8 => Ok(u64::from_le_bytes(data.try_into().unwrap())),
        Ok(_) => Ok(0), // malformed, ignore
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// Persist logical first_index to META file.
fn save_meta(dir: &Path, first_index: u64) -> Result<()> {
    let path = dir.join(META_FILE);
    let data = first_index.to_le_bytes();
    fs::write(&path, data)?;
    Ok(())
}

/// Scan a directory for segment files, returning them sorted by start_index with entry counts.
fn load_segments(dir: &Path) -> Result<Vec<SegmentInfo>> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(start_index) = parse_segment_filename(&name_str) {
            let path = entry.path();
            let (entry_count, _valid_offset) = validate_segment(&path)?;
            segments.push(SegmentInfo { path, start_index, entry_count });
        }
    }
    segments.sort_by_key(|s| s.start_index);
    Ok(segments)
}

/// Scan a segment file, validating CRC of each entry.
/// Returns `(valid_entry_count, valid_byte_offset)`.
fn validate_segment(path: &Path) -> Result<(u64, u64)> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut offset: u64 = 0;
    let mut count: u64 = 0;

    loop {
        // Need at least ENTRY_HEADER_SIZE bytes for the next header.
        if offset + ENTRY_HEADER_SIZE > file_len {
            break;
        }

        let data_len = match read_u32_le(&mut reader) {
            Ok(v) => v,
            Err(_) => break,
        };
        let stored_crc = match read_u32_le(&mut reader) {
            Ok(v) => v,
            Err(_) => break,
        };

        let entry_data_end = offset + ENTRY_HEADER_SIZE + data_len as u64;
        if entry_data_end > file_len {
            break; // partial data
        }

        let mut data = vec![0u8; data_len as usize];
        if reader.read_exact(&mut data).is_err() {
            break;
        }

        let computed_crc = crc32fast::hash(&data);
        if computed_crc != stored_crc {
            break; // CRC mismatch — treat as corruption boundary
        }

        offset = entry_data_end;
        count += 1;
    }

    Ok((count, offset))
}

/// Truncate a segment file to only its valid entries, returning the valid entry count.
fn recover_corrupted_segment(path: &Path) -> Result<u64> {
    let (valid_count, valid_offset) = validate_segment(path)?;
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(valid_offset)?;
    file.sync_all()?;
    Ok(valid_count)
}

/// Compute the byte offset after `entry_count` entries from the start of a segment file.
fn compute_byte_offset(path: &Path, entry_count: u64) -> Result<u64> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut offset: u64 = 0;

    for _ in 0..entry_count {
        let data_len = read_u32_le(&mut reader)?;
        let _crc = read_u32_le(&mut reader)?;
        // Skip data.
        reader.seek(SeekFrom::Current(data_len as i64))?;
        offset += ENTRY_HEADER_SIZE + data_len as u64;
    }

    Ok(offset)
}

fn open_segment_writer(path: &Path) -> Result<BufWriter<File>> {
    let file = OpenOptions::new().append(true).open(path)?;
    Ok(BufWriter::new(file))
}

fn read_u32_le(reader: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn default_opts() -> WalLogOptions {
        WalLogOptions { no_sync: true, ..Default::default() }
    }

    #[test]
    fn test_create_write_read() {
        let dir = tmp_dir();
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();

        wal.write(1, b"hello").unwrap();
        wal.write(2, b"world").unwrap();
        wal.write(3, b"!").unwrap();

        assert_eq!(wal.read(1).unwrap(), b"hello");
        assert_eq!(wal.read(2).unwrap(), b"world");
        assert_eq!(wal.read(3).unwrap(), b"!");
    }

    #[test]
    fn test_multiple_entries_indexing() {
        let dir = tmp_dir();
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();

        for i in 1..=10 {
            wal.write(i, format!("entry-{i}").as_bytes()).unwrap();
        }

        assert_eq!(wal.first_index(), 1);
        assert_eq!(wal.last_index(), 10);

        // Random reads.
        assert_eq!(wal.read(5).unwrap(), b"entry-5");
        assert_eq!(wal.read(1).unwrap(), b"entry-1");
        assert_eq!(wal.read(10).unwrap(), b"entry-10");
        assert_eq!(wal.read(7).unwrap(), b"entry-7");
    }

    #[test]
    fn test_reopen_persistence() {
        let dir = tmp_dir();
        {
            let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();
            wal.write(1, b"persist-a").unwrap();
            wal.write(2, b"persist-b").unwrap();
            wal.close().unwrap();
        }

        // Reopen and verify data persists.
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();
        assert_eq!(wal.first_index(), 1);
        assert_eq!(wal.last_index(), 2);
        assert_eq!(wal.read(1).unwrap(), b"persist-a");
        assert_eq!(wal.read(2).unwrap(), b"persist-b");
    }

    #[test]
    fn test_crc_corruption_recovery() {
        let dir = tmp_dir();
        {
            let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();
            wal.write(1, b"good-entry").unwrap();
            wal.write(2, b"will-corrupt").unwrap();
            wal.close().unwrap();
        }

        // Corrupt the last few bytes of the segment file.
        let seg_path = dir.path().join(segment_filename(1));
        let mut data = fs::read(&seg_path).unwrap();
        let len = data.len();
        // Flip some bytes near the end.
        for b in data[len - 4..].iter_mut() {
            *b ^= 0xFF;
        }
        fs::write(&seg_path, &data).unwrap();

        // Reopen — should recover by truncating the corrupted entry.
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();
        assert_eq!(wal.first_index(), 1);
        assert_eq!(wal.last_index(), 1);
        assert_eq!(wal.read(1).unwrap(), b"good-entry");
        assert!(wal.read(2).is_err());
    }

    #[test]
    fn test_truncate_back() {
        let dir = tmp_dir();
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();

        for i in 1..=5 {
            wal.write(i, format!("entry-{i}").as_bytes()).unwrap();
        }

        wal.truncate_back(3).unwrap();
        assert_eq!(wal.last_index(), 3);
        assert_eq!(wal.read(1).unwrap(), b"entry-1");
        assert_eq!(wal.read(3).unwrap(), b"entry-3");
        assert!(wal.read(4).is_err());
        assert!(wal.read(5).is_err());
    }

    #[test]
    fn test_truncate_front() {
        let dir = tmp_dir();
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();

        for i in 1..=5 {
            wal.write(i, format!("entry-{i}").as_bytes()).unwrap();
        }

        wal.truncate_front(3).unwrap();
        assert_eq!(wal.first_index(), 3);
        assert!(wal.read(1).is_err());
        assert!(wal.read(2).is_err());
        assert_eq!(wal.read(3).unwrap(), b"entry-3");
        assert_eq!(wal.read(5).unwrap(), b"entry-5");
    }

    #[test]
    fn test_segment_rollover() {
        let dir = tmp_dir();
        let opts = WalLogOptions { no_sync: true, segment_size: 128 };
        let mut wal = WalLog::open(dir.path(), opts.clone()).unwrap();

        // Write enough entries to trigger multiple segments.
        // Each entry: 8 bytes header + data. With ~50-byte payloads, ~2 entries per 128-byte
        // segment.
        for i in 1..=20 {
            let payload = format!("segment-rollover-payload-{i:04}");
            wal.write(i, payload.as_bytes()).unwrap();
        }

        assert_eq!(wal.first_index(), 1);
        assert_eq!(wal.last_index(), 20);
        assert!(wal.segments.len() > 1, "expected multiple segments, got {}", wal.segments.len());

        // Verify all entries can be read across segments.
        for i in 1..=20 {
            let expected = format!("segment-rollover-payload-{i:04}");
            assert_eq!(wal.read(i).unwrap(), expected.as_bytes());
        }

        // Reopen and verify cross-segment reads still work.
        wal.close().unwrap();
        let mut wal2 = WalLog::open(dir.path(), opts).unwrap();
        assert_eq!(wal2.first_index(), 1);
        assert_eq!(wal2.last_index(), 20);
        for i in 1..=20 {
            let expected = format!("segment-rollover-payload-{i:04}");
            assert_eq!(wal2.read(i).unwrap(), expected.as_bytes());
        }
    }

    #[test]
    fn test_empty_log() {
        let dir = tmp_dir();
        let mut wal = WalLog::open(dir.path(), default_opts()).unwrap();

        assert_eq!(wal.first_index(), 0);
        assert_eq!(wal.last_index(), 0);
        assert!(wal.read(1).is_err());
        assert!(wal.read(0).is_err());
    }
}
