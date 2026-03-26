use alloy_primitives::B256;
use mptdb_common::error::{MptDbError, Result};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub(crate) const SHARED_PAGES_DATA_MAGIC: &[u8; 8] = b"stpgd001";
pub(crate) const SHARED_PAGES_INDEX_MAGIC: &[u8; 8] = b"stpgi001";
pub(crate) const FLAT_PAGE_MAGIC: &[u8; 8] = b"stpgx001";
pub(crate) const FLAT_LAYOUT_VERSION: u16 = 1;
pub(crate) const FLAT_PAGE_FEATURE_FLAGS: u16 = 0;
pub(crate) const FLAT_PAGE_HEADER_LEN: usize = 64;
pub(crate) const PAGE_INDEX_RECORD_LEN: usize = 8 + 4 + 32 + 4 + 2 + 2 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FlatPageHeader {
    pub layout_version: u16,
    pub feature_flags: u16,
    pub total_len: u32,
    pub root_record_off: u32,
    pub payload_off: u32,
    pub root: B256,
    pub checksum: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FlatPageIndexEntry {
    pub page_off: u64,
    pub total_len: u32,
    pub root: B256,
    pub root_record_off: u32,
    pub layout_version: u16,
    pub feature_flags: u16,
    pub checksum: u32,
}

pub(crate) fn data_path(dir: &Path) -> PathBuf {
    dir.join("pages.data")
}

pub(crate) fn index_path(dir: &Path) -> PathBuf {
    dir.join("pages.index")
}

pub(crate) fn open_data_file(path: &Path) -> Result<File> {
    open_with_magic(path, SHARED_PAGES_DATA_MAGIC)
}

pub(crate) fn open_index_file(path: &Path) -> Result<File> {
    open_with_magic(path, SHARED_PAGES_INDEX_MAGIC)
}

pub(crate) fn open_with_magic(path: &Path, magic: &[u8; 8]) -> Result<File> {
    let mut file =
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(
            |e| MptDbError::Other(format!("open flat store file {}: {e}", path.display())),
        )?;

    let len = file
        .metadata()
        .map_err(|e| MptDbError::Other(format!("stat flat store file {}: {e}", path.display())))?
        .len();

    if len == 0 {
        file.write_all(magic)
            .map_err(|e| MptDbError::Other(format!("write flat store header: {e}")))?;
        file.flush().map_err(|e| MptDbError::Other(format!("flush flat store header: {e}")))?;
    } else {
        let mut header = [0u8; 8];
        file.seek(SeekFrom::Start(0))
            .map_err(|e| MptDbError::Other(format!("seek flat store header: {e}")))?;
        file.read_exact(&mut header)
            .map_err(|e| MptDbError::Other(format!("read flat store header: {e}")))?;
        if &header != magic {
            return Err(MptDbError::Other(format!(
                "unexpected flat store header in {}",
                path.display()
            )));
        }
    }

    Ok(file)
}

pub(crate) fn encode_page(payload: &[u8], root: B256, root_record_off: u32) -> Vec<u8> {
    let total_len = (FLAT_PAGE_HEADER_LEN + payload.len()) as u32;
    let checksum = checksum32(payload);

    let mut out = Vec::with_capacity(total_len as usize);
    out.extend_from_slice(FLAT_PAGE_MAGIC);
    out.extend_from_slice(&FLAT_LAYOUT_VERSION.to_le_bytes());
    out.extend_from_slice(&FLAT_PAGE_FEATURE_FLAGS.to_le_bytes());
    out.extend_from_slice(&total_len.to_le_bytes());
    out.extend_from_slice(&root_record_off.to_le_bytes());
    out.extend_from_slice(&(FLAT_PAGE_HEADER_LEN as u32).to_le_bytes());
    out.extend_from_slice(root.as_slice());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&[0u8; FLAT_PAGE_HEADER_LEN - 8 - 2 - 2 - 4 - 4 - 4 - 32 - 4]);
    out.extend_from_slice(payload);
    out
}

/// Lightweight header read for the L3 open hot path.
///
/// Performs all structural checks (magic, version, bounds, offsets) but skips
/// the `checksum32(payload)` scan.  Scanning the full payload (~7KB per trie)
/// on every L3 open costs ~5200ms sum / 8K opens per B4.6 block.
///
/// Full CRC is validated at publish time (`read_page_header` in publish path).
/// Corruption that slips past publish-time validation is caught by a future
/// background scrub worker.
///
/// Retained checks (bounds + structural integrity):
/// - minimum length, magic bytes, layout version
/// - `total_len` fits in the slice
/// - `payload_off` is in valid range
pub(crate) fn read_page_header_light(bytes: &[u8]) -> Result<FlatPageHeader> {
    if bytes.len() < FLAT_PAGE_HEADER_LEN {
        return Err(MptDbError::Other("flat page too short".to_string()));
    }
    if &bytes[..8] != FLAT_PAGE_MAGIC {
        return Err(MptDbError::Other("invalid flat page magic".to_string()));
    }

    let layout_version = u16::from_le_bytes([bytes[8], bytes[9]]);
    let feature_flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    let total_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let root_record_off = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let payload_off = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let root = B256::from_slice(&bytes[24..56]);
    let checksum = u32::from_le_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);

    if layout_version != FLAT_LAYOUT_VERSION {
        return Err(MptDbError::Other(format!("unsupported flat layout version: {layout_version}")));
    }
    if total_len as usize > bytes.len() {
        return Err(MptDbError::Other("flat page length out of bounds".to_string()));
    }
    if payload_off as usize > total_len as usize || (payload_off as usize) < FLAT_PAGE_HEADER_LEN {
        return Err(MptDbError::Other("flat page payload offset out of bounds".to_string()));
    }
    // Payload CRC intentionally skipped — see doc comment above.

    Ok(FlatPageHeader {
        layout_version,
        feature_flags,
        total_len,
        root_record_off,
        payload_off,
        root,
        checksum,
    })
}

pub(crate) fn read_page_header(bytes: &[u8]) -> Result<FlatPageHeader> {
    if bytes.len() < FLAT_PAGE_HEADER_LEN {
        return Err(MptDbError::Other("flat page too short".to_string()));
    }
    if &bytes[..8] != FLAT_PAGE_MAGIC {
        return Err(MptDbError::Other("invalid flat page magic".to_string()));
    }

    let layout_version = u16::from_le_bytes([bytes[8], bytes[9]]);
    let feature_flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    let total_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let root_record_off = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let payload_off = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let root = B256::from_slice(&bytes[24..56]);
    let checksum = u32::from_le_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);

    if layout_version != FLAT_LAYOUT_VERSION {
        return Err(MptDbError::Other(format!("unsupported flat layout version: {layout_version}")));
    }
    if total_len as usize > bytes.len() {
        return Err(MptDbError::Other("flat page length out of bounds".to_string()));
    }
    if payload_off as usize > total_len as usize || (payload_off as usize) < FLAT_PAGE_HEADER_LEN {
        return Err(MptDbError::Other("flat page payload offset out of bounds".to_string()));
    }

    let payload = &bytes[payload_off as usize..total_len as usize];
    if checksum32(payload) != checksum {
        return Err(MptDbError::Other("flat page checksum mismatch".to_string()));
    }

    Ok(FlatPageHeader {
        layout_version,
        feature_flags,
        total_len,
        root_record_off,
        payload_off,
        root,
        checksum,
    })
}

pub(crate) fn append_page(
    data_file: &mut File,
    index_file: &mut File,
    page_bytes: &[u8],
    root: B256,
    root_record_off: u32,
) -> Result<FlatPageIndexEntry> {
    let header = read_page_header(page_bytes)?;
    if header.root != root || header.root_record_off != root_record_off {
        return Err(MptDbError::Other("flat page header mismatch".to_string()));
    }

    let page_off = data_file
        .seek(SeekFrom::End(0))
        .map_err(|e| MptDbError::Other(format!("seek flat data append: {e}")))?;
    data_file
        .write_all(page_bytes)
        .map_err(|e| MptDbError::Other(format!("append flat page data: {e}")))?;

    let entry = FlatPageIndexEntry {
        page_off,
        total_len: header.total_len,
        root,
        root_record_off,
        layout_version: header.layout_version,
        feature_flags: header.feature_flags,
        checksum: header.checksum,
    };
    append_page_index_record(index_file, &entry)?;
    Ok(entry)
}

pub(crate) fn append_page_index_record(
    index_file: &mut File,
    entry: &FlatPageIndexEntry,
) -> Result<()> {
    index_file
        .seek(SeekFrom::End(0))
        .map_err(|e| MptDbError::Other(format!("seek flat page index append: {e}")))?;
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
    index_file
        .write_all(&record)
        .map_err(|e| MptDbError::Other(format!("append flat page index record: {e}")))?;
    index_file
        .flush()
        .map_err(|e| MptDbError::Other(format!("flush flat page index record: {e}")))?;
    Ok(())
}

pub(crate) fn write_full_page_index(path: &Path, entries: &[FlatPageIndexEntry]) -> Result<()> {
    let mut file = File::create(path)
        .map_err(|e| MptDbError::Other(format!("create flat page index tmp: {e}")))?;
    file.write_all(SHARED_PAGES_INDEX_MAGIC)
        .map_err(|e| MptDbError::Other(format!("write flat page index header: {e}")))?;
    for entry in entries {
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
        file.write_all(&record)
            .map_err(|e| MptDbError::Other(format!("write flat page index record: {e}")))?;
    }
    file.flush().map_err(|e| MptDbError::Other(format!("flush flat page index tmp: {e}")))?;
    Ok(())
}

pub(crate) fn load_page_index(index_file: &mut File) -> Result<Vec<FlatPageIndexEntry>> {
    index_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| MptDbError::Other(format!("seek flat page index: {e}")))?;
    let mut bytes = Vec::new();
    index_file
        .read_to_end(&mut bytes)
        .map_err(|e| MptDbError::Other(format!("read flat page index: {e}")))?;
    if bytes.len() < SHARED_PAGES_INDEX_MAGIC.len() ||
        &bytes[..SHARED_PAGES_INDEX_MAGIC.len()] != SHARED_PAGES_INDEX_MAGIC
    {
        return Err(MptDbError::Other("invalid flat page index header".to_string()));
    }

    let mut entries = Vec::new();
    let mut pos = SHARED_PAGES_INDEX_MAGIC.len();
    while pos + PAGE_INDEX_RECORD_LEN <= bytes.len() {
        let page_off = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let total_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let root = B256::from_slice(&bytes[pos..pos + 32]);
        pos += 32;
        let root_record_off = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let layout_version = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
        pos += 2;
        let feature_flags = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
        pos += 2;
        let checksum = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        entries.push(FlatPageIndexEntry {
            page_off,
            total_len,
            root,
            root_record_off,
            layout_version,
            feature_flags,
            checksum,
        });
    }
    Ok(entries)
}

fn checksum32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16777619);
    }
    hash
}
