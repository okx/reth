// Generated protobuf types for mptdb.
include!(concat!(env!("OUT_DIR"), "/mptdb.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn test_changeset_roundtrip() {
        let cs = ChangeSet {
            pairs: vec![
                KvPair { delete: false, key: b"key1".to_vec(), value: b"val1".to_vec() },
                KvPair { delete: true, key: b"key2".to_vec(), value: vec![] },
            ],
        };

        let encoded = cs.encode_to_vec();
        let decoded = ChangeSet::decode(encoded.as_slice()).expect("decode failed");
        assert_eq!(decoded.pairs.len(), 2);
        assert!(!decoded.pairs[0].delete);
        assert_eq!(decoded.pairs[0].key, b"key1");
        assert_eq!(decoded.pairs[0].value, b"val1");
        assert!(decoded.pairs[1].delete);
        assert_eq!(decoded.pairs[1].key, b"key2");
    }

    #[test]
    fn test_changelog_entry_roundtrip() {
        let entry = ChangelogEntry {
            version: 100,
            changeset: Some(ChangeSet {
                pairs: vec![
                    KvPair { delete: false, key: b"key1".to_vec(), value: b"val1".to_vec() },
                    KvPair { delete: true, key: b"key2".to_vec(), value: vec![] },
                ],
            }),
        };

        let encoded = entry.encode_to_vec();
        let decoded = ChangelogEntry::decode(encoded.as_slice()).expect("decode failed");
        assert_eq!(decoded.version, 100);
        let cs = decoded.changeset.unwrap();
        assert_eq!(cs.pairs.len(), 2);
        assert!(!cs.pairs[0].delete);
        assert_eq!(cs.pairs[0].key, b"key1");
        assert!(cs.pairs[1].delete);
    }
}
