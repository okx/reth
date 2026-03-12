// Generated protobuf types for seidb.
include!(concat!(env!("OUT_DIR"), "/seidb.rs"));

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommitID{{version:{} hash:", self.version)?;
        for byte in &self.hash {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn test_commit_id_display() {
        let id = CommitId { version: 42, hash: vec![0xde, 0xad, 0xbe, 0xef] };
        assert_eq!(id.to_string(), "CommitID{version:42 hash:deadbeef}");

        let empty = CommitId { version: 0, hash: vec![] };
        assert_eq!(empty.to_string(), "CommitID{version:0 hash:}");
    }

    #[test]
    fn test_changelog_entry_roundtrip() {
        let entry = ChangelogEntry {
            version: 100,
            changesets: vec![NamedChangeSet {
                changeset: Some(ChangeSet {
                    pairs: vec![
                        KvPair { delete: false, key: b"key1".to_vec(), value: b"val1".to_vec() },
                        KvPair { delete: true, key: b"key2".to_vec(), value: vec![] },
                    ],
                }),
                name: "store_a".to_string(),
            }],
            upgrades: vec![TreeNameUpgrade {
                name: "new_tree".to_string(),
                rename_from: "old_tree".to_string(),
                delete: false,
            }],
        };

        let encoded = entry.encode_to_vec();
        let decoded = ChangelogEntry::decode(encoded.as_slice()).expect("decode failed");
        assert_eq!(decoded.version, 100);
        assert_eq!(decoded.changesets.len(), 1);
        assert_eq!(decoded.changesets[0].name, "store_a");
        let pairs = &decoded.changesets[0].changeset.as_ref().unwrap().pairs;
        assert_eq!(pairs.len(), 2);
        assert!(!pairs[0].delete);
        assert_eq!(pairs[0].key, b"key1");
        assert!(pairs[1].delete);
        assert_eq!(decoded.upgrades.len(), 1);
        assert_eq!(decoded.upgrades[0].name, "new_tree");
    }

    #[test]
    fn test_named_changeset_roundtrip() {
        let ncs = NamedChangeSet {
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    delete: false,
                    key: b"hello".to_vec(),
                    value: b"world".to_vec(),
                }],
            }),
            name: "my_store".to_string(),
        };

        let encoded = ncs.encode_to_vec();
        let decoded = NamedChangeSet::decode(encoded.as_slice()).expect("decode failed");
        assert_eq!(decoded.name, "my_store");
        let cs = decoded.changeset.unwrap();
        assert_eq!(cs.pairs.len(), 1);
        assert_eq!(cs.pairs[0].key, b"hello");
        assert_eq!(cs.pairs[0].value, b"world");
    }
}
