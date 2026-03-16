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
    use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use mptdb_traits::wal::Wal;

    #[test]
    fn test_changelog_write_multiple_changesets() {
        let dir = tempfile::tempdir().unwrap();
        let config = WalConfig { fsync_enabled: false, ..Default::default() };

        let wal = new_changelog_wal(config, dir.path()).unwrap();

        let entry = ChangelogEntry {
            version: 42,
            changesets: vec![
                NamedChangeSet {
                    name: "store_a".to_string(),
                    changeset: Some(ChangeSet {
                        pairs: vec![KvPair {
                            delete: false,
                            key: b"k1".to_vec(),
                            value: b"v1".to_vec(),
                        }],
                    }),
                },
                NamedChangeSet {
                    name: "store_b".to_string(),
                    changeset: Some(ChangeSet {
                        pairs: vec![KvPair { delete: true, key: b"k2".to_vec(), value: vec![] }],
                    }),
                },
                NamedChangeSet {
                    name: "store_c".to_string(),
                    changeset: Some(ChangeSet {
                        pairs: vec![
                            KvPair { delete: false, key: b"k3".to_vec(), value: b"v3".to_vec() },
                            KvPair { delete: false, key: b"k4".to_vec(), value: b"v4".to_vec() },
                        ],
                    }),
                },
            ],
            upgrades: vec![],
        };

        wal.write(entry).unwrap();

        let read_back = wal.read_at(1).unwrap();
        assert_eq!(read_back.version, 42);
        assert_eq!(read_back.changesets.len(), 3);

        assert_eq!(read_back.changesets[0].name, "store_a");
        let pairs_a = &read_back.changesets[0].changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs_a.len(), 1);
        assert!(!pairs_a[0].delete);
        assert_eq!(pairs_a[0].key, b"k1");
        assert_eq!(pairs_a[0].value, b"v1");

        assert_eq!(read_back.changesets[1].name, "store_b");
        let pairs_b = &read_back.changesets[1].changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs_b.len(), 1);
        assert!(pairs_b[0].delete);
        assert_eq!(pairs_b[0].key, b"k2");

        assert_eq!(read_back.changesets[2].name, "store_c");
        let pairs_c = &read_back.changesets[2].changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs_c.len(), 2);
        assert_eq!(pairs_c[0].key, b"k3");
        assert_eq!(pairs_c[0].value, b"v3");
        assert_eq!(pairs_c[1].key, b"k4");
        assert_eq!(pairs_c[1].value, b"v4");
    }
}
