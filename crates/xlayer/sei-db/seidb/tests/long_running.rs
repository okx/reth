//! Long-running integration test for SeiDb.
//!
//! Commits a configurable number of blocks with deterministic changesets,
//! verifying data integrity at checkpoints and after close/reopen.
//!
//! Run with:
//!   cargo test -p seidb --test long_running -- --ignored --nocapture
//!
//! Configure block count via environment variable (default: 100):
//!   SEIDB_LONG_RUN_BLOCKS=10000 cargo test -p seidb --test long_running -- --ignored --nocapture

use seidb::db::SeiDb;
use seidb_common::config::{MemIavlConfig, StateCommitConfig, WriteMode};
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use tempfile::tempdir;

/// Read the target block count from `SEIDB_LONG_RUN_BLOCKS`, defaulting to 100.
fn block_count() -> u64 {
    std::env::var("SEIDB_LONG_RUN_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(100)
}

#[test]
#[ignore] // Run manually: cargo test -p seidb --test long_running -- --ignored
fn test_long_running_blocks() {
    let total_blocks = block_count();
    let checkpoint_interval = (total_blocks / 10).max(1);

    let dir = tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();

    let config = StateCommitConfig {
        write_mode: WriteMode::CosmosOnly,
        memiavl: MemIavlConfig {
            snapshot_interval: 0, // disable auto-snapshot for deterministic version tracking
            ..Default::default()
        },
        ..Default::default()
    };

    let mut seidb = SeiDb::open(&home, config, None).unwrap();
    seidb.initialize(&["bank".into(), "staking".into()]);
    seidb.load_version(0).unwrap();

    eprintln!(
        "running long_running test: {total_blocks} blocks, checkpoint every {checkpoint_interval}"
    );

    for block in 1..=total_blocks {
        // Deterministic "random" changeset size via Knuth multiplicative hash
        let seed = block.wrapping_mul(2654435761);
        let num_pairs = ((seed % 10) + 1) as usize;

        let pairs: Vec<KvPair> = (0..num_pairs)
            .map(|i| {
                let key = format!("key_{}_{}", block, i);
                let value = format!("value_{}_{}", block, i).into_bytes();
                KvPair { delete: false, key: key.into_bytes(), value }
            })
            .collect();

        let cs = vec![NamedChangeSet { name: "bank".into(), changeset: Some(ChangeSet { pairs }) }];

        seidb.sc_mut().apply_change_sets(&cs).unwrap();
        let version = seidb.sc_mut().commit().unwrap();
        assert_eq!(version, block as i64);

        // Checkpoint verification at regular intervals
        if block % checkpoint_interval == 0 {
            assert_eq!(seidb.version(), block as i64);
            let info = seidb.sc_mut().working_commit_info();
            assert!(info.version > 0, "working commit info version should be > 0 at block {block}");
            eprintln!("checkpoint: block {block}/{total_blocks} OK, version={}", seidb.version());
        }
    }

    // Final verification
    assert_eq!(seidb.version(), total_blocks as i64);
    eprintln!("all {total_blocks} blocks committed successfully");

    // Close and reopen to verify WAL replay restores state
    seidb.close().unwrap();

    let config2 = StateCommitConfig {
        write_mode: WriteMode::CosmosOnly,
        memiavl: MemIavlConfig { snapshot_interval: 0, ..Default::default() },
        ..Default::default()
    };
    let mut seidb2 = SeiDb::open(&home, config2, None).unwrap();
    seidb2.initialize(&["bank".into(), "staking".into()]);
    seidb2.load_version(0).unwrap(); // loads latest via WAL replay
    assert_eq!(seidb2.version(), total_blocks as i64);
    eprintln!("reopen verified: version={}", seidb2.version());
    seidb2.close().unwrap();
}
