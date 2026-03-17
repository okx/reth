use crate::wal::WalImpl;
use mptdb_common::{
    config::WalConfig,
    error::{MptDbError, Result},
};
use mptdb_proto::ChangelogEntry;
use prost::Message;
use std::path::Path;

/// Type alias for a WAL specialized for ChangelogEntry.
pub type ChangelogWal = WalImpl<ChangelogEntry>;

/// Create a new ChangelogWal with protobuf serialization.
pub fn new_changelog_wal(config: WalConfig, dir: impl AsRef<Path>) -> Result<ChangelogWal> {
    WalImpl::new(
        |entry: &ChangelogEntry| {
            let mut buf = Vec::with_capacity(entry.encoded_len());
            entry.encode(&mut buf).map_err(|e| MptDbError::Other(format!("proto encode: {e}")))?;
            Ok(buf)
        },
        |data: &[u8]| ChangelogEntry::decode(data).map_err(MptDbError::Proto),
        config,
        dir,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_proto::{ChangeSet, KvPair};
    use mptdb_traits::wal::Wal;

    #[test]
    fn test_changelog_write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = WalConfig { fsync_enabled: false, ..Default::default() };

        let wal = new_changelog_wal(config, dir.path()).unwrap();

        let entry = ChangelogEntry {
            version: 42,
            changeset: Some(ChangeSet {
                pairs: vec![
                    KvPair { delete: false, key: b"k1".to_vec(), value: b"v1".to_vec() },
                    KvPair { delete: true, key: b"k2".to_vec(), value: vec![] },
                    KvPair { delete: false, key: b"k3".to_vec(), value: b"v3".to_vec() },
                    KvPair { delete: false, key: b"k4".to_vec(), value: b"v4".to_vec() },
                ],
            }),
        };

        wal.write(entry).unwrap();

        let read_back = wal.read_at(1).unwrap();
        assert_eq!(read_back.version, 42);

        let pairs = &read_back.changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs.len(), 4);

        assert!(!pairs[0].delete);
        assert_eq!(pairs[0].key, b"k1");
        assert_eq!(pairs[0].value, b"v1");

        assert!(pairs[1].delete);
        assert_eq!(pairs[1].key, b"k2");

        assert!(!pairs[2].delete);
        assert_eq!(pairs[2].key, b"k3");
        assert_eq!(pairs[2].value, b"v3");

        assert!(!pairs[3].delete);
        assert_eq!(pairs[3].key, b"k4");
        assert_eq!(pairs[3].value, b"v4");
    }
}
