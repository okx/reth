//! Phase 2 integration tests (T2.CP).

use mptdb_common::config::WalConfig;
use mptdb_proto::{ChangeSet, ChangelogEntry, KvPair};
use mptdb_traits::wal::Wal;
use mptdb_wal::{changelog::new_changelog_wal, wal::WalImpl};
use std::time::Duration;
use tempfile::tempdir;

/// Helper: build a ChangelogEntry with `n` KV pairs in a single changeset.
fn make_entry(version: i64, n: usize) -> ChangelogEntry {
    ChangelogEntry {
        version,
        changeset: Some(ChangeSet {
            pairs: (0..n)
                .map(|i| KvPair {
                    delete: false,
                    key: format!("key-{version}-{i}").into_bytes(),
                    value: format!("val-{version}-{i}").into_bytes(),
                })
                .collect(),
        }),
    }
}

#[test]
fn test_corruption_recovery_end_to_end() {
    let dir = tempdir().unwrap();
    let wal_dir = dir.path().join("wal");

    // Write 5 entries.
    {
        let config = WalConfig::default();
        let mut wal = new_changelog_wal(config, &wal_dir).unwrap();
        for v in 1..=5 {
            wal.write(make_entry(v, 2)).unwrap();
        }
        wal.close().unwrap();
    }

    // Corrupt the last entry: find the segment file and tamper with trailing bytes.
    {
        let mut entries: Vec<_> = std::fs::read_dir(&wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let last_seg = entries.last().unwrap().path();
        let mut data = std::fs::read(&last_seg).unwrap();
        // Flip some bytes near the end.
        let len = data.len();
        if len > 10 {
            data[len - 5] ^= 0xFF;
            data[len - 3] ^= 0xFF;
        }
        std::fs::write(&last_seg, &data).unwrap();
    }

    // Reopen — corrupted entry should be truncated.
    {
        let config = WalConfig::default();
        let wal = new_changelog_wal(config, &wal_dir).unwrap();
        let last = wal.last_offset().unwrap();
        assert_eq!(last, 4, "corrupted 5th entry should be truncated, last={last}");
        // Verify first 4 are intact.
        for v in 1..=4 {
            let entry = wal.read_at(v as u64).unwrap();
            assert_eq!(entry.version, v);
            assert_eq!(entry.changeset.as_ref().unwrap().pairs.len(), 2);
        }
    }
}

#[test]
fn test_full_lifecycle() {
    let dir = tempdir().unwrap();
    let wal_dir = dir.path().join("wal");

    let async_config =
        WalConfig { write_buffer_size: 64, write_batch_size: 8, ..Default::default() };

    // Write 100 entries async.
    {
        let mut wal = new_changelog_wal(async_config.clone(), &wal_dir).unwrap();
        for v in 1..=100 {
            wal.write(make_entry(v, 1)).unwrap();
        }
        wal.close().unwrap();
    }

    // Reopen, replay all 100.
    {
        let mut wal = new_changelog_wal(async_config.clone(), &wal_dir).unwrap();
        let mut count = 0i64;
        wal.replay(1, 100, &mut |idx, entry: ChangelogEntry| {
            count += 1;
            assert_eq!(idx, count as u64);
            assert_eq!(entry.version, count);
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 100);

        // Truncate operations.
        wal.truncate_before(50).unwrap();
        assert_eq!(wal.first_offset().unwrap(), 50);

        wal.truncate_after(80).unwrap();
        assert_eq!(wal.last_offset().unwrap(), 80);

        // Write 10 more (indices 81..90).
        for v in 81..=90 {
            wal.write(make_entry(v, 1)).unwrap();
        }
        wal.close().unwrap();
    }

    // Reopen, verify final state.
    {
        let wal = new_changelog_wal(async_config, &wal_dir).unwrap();
        assert_eq!(wal.first_offset().unwrap(), 50);
        assert_eq!(wal.last_offset().unwrap(), 90);
    }
}

#[test]
fn test_prune_lifecycle() {
    let dir = tempdir().unwrap();
    let wal_dir = dir.path().join("wal");

    let config = WalConfig {
        write_buffer_size: 64,
        write_batch_size: 4,
        keep_recent: 10,
        prune_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let mut wal: WalImpl<String> = WalImpl::new(
        |s: &String| Ok(s.as_bytes().to_vec()),
        |data: &[u8]| {
            Ok(String::from_utf8(data.to_vec()).map_err(|e| MptDbError::Other(e.to_string()))?)
        },
        config,
        &wal_dir,
    )
    .unwrap();

    // Write 50 entries.
    for i in 1..=50 {
        wal.write(format!("entry-{i}")).unwrap();
    }

    // Wait for prune to kick in.
    std::thread::sleep(Duration::from_millis(200));

    let last = wal.last_offset().unwrap();
    let first = wal.first_offset().unwrap();
    assert_eq!(last, 50, "last should be 50");
    assert!(first >= 40, "prune should advance first to >= 40, got {first}");

    // Verify readable range is intact.
    for idx in first..=last {
        let val = wal.read_at(idx).unwrap();
        assert_eq!(val, format!("entry-{idx}"));
    }

    wal.close().unwrap();
}

use mptdb_common::error::MptDbError;
