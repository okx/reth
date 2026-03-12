// Parquet WAL — binary encode/decode for receipt WAL entries and WalImpl creation.
//
// The Parquet WAL stores receipt data in a simple length-prefixed binary format,
// independent of the SS ChangelogWAL. Each entry holds a block number and a list
// of serialized receipt byte arrays.

use seidb_common::{
    config::WalConfig,
    error::{Result, SeiDbError},
};
use seidb_wal::wal::WalImpl;
use std::path::Path;

/// WAL entry for the Parquet receipt store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalEntry {
    pub block_number: u64,
    pub receipts: Vec<Vec<u8>>,
}

/// Encode a [`WalEntry`] to binary.
///
/// Format: `block_number(8 LE) + count(4 LE) + for each receipt: len(4 LE) + data`
pub fn encode_wal_entry(entry: &WalEntry) -> Result<Vec<u8>> {
    let count: u32 = entry
        .receipts
        .len()
        .try_into()
        .map_err(|_| SeiDbError::Other(format!("receipt count exceeds u32: {}", entry.receipts.len())))?;

    // Pre-compute total size.
    let mut size = 8 + 4; // block_number + count
    for r in &entry.receipts {
        let rlen: u32 = r
            .len()
            .try_into()
            .map_err(|_| SeiDbError::Other(format!("receipt length exceeds u32: {}", r.len())))?;
        size += 4 + rlen as usize;
    }

    let mut buf = vec![0u8; size];
    let mut offset = 0;

    buf[offset..offset + 8].copy_from_slice(&entry.block_number.to_le_bytes());
    offset += 8;

    buf[offset..offset + 4].copy_from_slice(&count.to_le_bytes());
    offset += 4;

    for r in &entry.receipts {
        let rlen = r.len() as u32;
        buf[offset..offset + 4].copy_from_slice(&rlen.to_le_bytes());
        offset += 4;
        buf[offset..offset + r.len()].copy_from_slice(r);
        offset += r.len();
    }

    Ok(buf)
}

/// Decode a [`WalEntry`] from binary produced by [`encode_wal_entry`].
pub fn decode_wal_entry(data: &[u8]) -> Result<WalEntry> {
    if data.len() < 12 {
        return Err(SeiDbError::Other(format!("WAL entry too short: {} bytes", data.len())));
    }

    let mut offset = 0;

    let block_number = u64::from_le_bytes(
        data[offset..offset + 8]
            .try_into()
            .map_err(|_| SeiDbError::Other("failed to read block_number".into()))?,
    );
    offset += 8;

    let num_receipts = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .map_err(|_| SeiDbError::Other("failed to read receipt count".into()))?,
    );
    offset += 4;

    let mut receipts = Vec::with_capacity(num_receipts as usize);
    for i in 0..num_receipts {
        if offset + 4 > data.len() {
            return Err(SeiDbError::Other(format!(
                "WAL entry truncated at receipt {i} length"
            )));
        }
        let rlen = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| SeiDbError::Other(format!("failed to read receipt {i} length")))?,
        ) as usize;
        offset += 4;

        if offset + rlen > data.len() {
            return Err(SeiDbError::Other(format!(
                "WAL entry truncated at receipt {i} data"
            )));
        }
        receipts.push(data[offset..offset + rlen].to_vec());
        offset += rlen;
    }

    Ok(WalEntry { block_number, receipts })
}

/// Create a new [`WalImpl<WalEntry>`] backed by the given directory.
///
/// Uses default [`WalConfig`] (synchronous mode, no auto-prune).
pub fn new_parquet_wal(dir: impl AsRef<Path>) -> Result<WalImpl<WalEntry>> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    WalImpl::new(
        |entry: &WalEntry| encode_wal_entry(entry),
        |data: &[u8]| decode_wal_entry(data),
        WalConfig::default(),
        dir,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_traits::wal::Wal;

    #[test]
    fn test_wal_entry_roundtrip() {
        let entry = WalEntry {
            block_number: 42,
            receipts: vec![vec![1, 2, 3], vec![4, 5]],
        };
        let encoded = encode_wal_entry(&entry).unwrap();
        let decoded = decode_wal_entry(&encoded).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn test_wal_entry_empty_receipts() {
        let entry = WalEntry {
            block_number: 100,
            receipts: vec![],
        };
        let encoded = encode_wal_entry(&entry).unwrap();
        let decoded = decode_wal_entry(&encoded).unwrap();
        assert_eq!(entry, decoded);
        // 8 (block_number) + 4 (count=0)
        assert_eq!(encoded.len(), 12);
    }

    #[test]
    fn test_wal_entry_multiple_receipts() {
        let entry = WalEntry {
            block_number: u64::MAX,
            receipts: vec![
                vec![0xAA; 256],
                vec![0xBB; 1],
                vec![],
                vec![0xCC; 1024],
            ],
        };
        let encoded = encode_wal_entry(&entry).unwrap();
        let decoded = decode_wal_entry(&encoded).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn test_parquet_wal_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_parquet_wal(dir.path()).unwrap();

        let entry1 = WalEntry {
            block_number: 1,
            receipts: vec![vec![10, 20], vec![30]],
        };
        let entry2 = WalEntry {
            block_number: 2,
            receipts: vec![vec![40, 50, 60]],
        };

        wal.write(entry1.clone()).unwrap();
        wal.write(entry2.clone()).unwrap();

        let read1 = wal.read_at(1).unwrap();
        let read2 = wal.read_at(2).unwrap();

        assert_eq!(read1, entry1);
        assert_eq!(read2, entry2);
    }
}
