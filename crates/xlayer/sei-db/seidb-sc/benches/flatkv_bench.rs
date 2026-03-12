use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use seidb_common::config::FlatKvConfig;
use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
use seidb_sc::flatkv::{
    lthash::LtHash,
    lthash_compute::{compute_lt_hash, KvPairWithLastValue},
    store::CommitStore,
};
use tempfile::tempdir;

fn bench_lthash_serial(c: &mut Criterion) {
    let pairs: Vec<KvPairWithLastValue> = (0..50)
        .map(|i| KvPairWithLastValue {
            key: format!("key_{i:04}").into_bytes(),
            value: vec![0u8; 64],
            last_value: vec![],
            delete: false,
        })
        .collect();
    let prev = LtHash::new();

    c.bench_function("lthash_serial_50", |b| {
        b.iter(|| compute_lt_hash(&prev, &pairs));
    });
}

fn bench_lthash_parallel(c: &mut Criterion) {
    let pairs: Vec<KvPairWithLastValue> = (0..500)
        .map(|i| KvPairWithLastValue {
            key: format!("key_{i:04}").into_bytes(),
            value: vec![0u8; 64],
            last_value: vec![],
            delete: false,
        })
        .collect();
    let prev = LtHash::new();

    c.bench_function("lthash_parallel_500", |b| {
        b.iter(|| compute_lt_hash(&prev, &pairs));
    });
}

fn bench_flatkv_apply_commit(c: &mut Criterion) {
    c.bench_function("flatkv_apply_commit_block", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().unwrap();
                let mut store =
                    CommitStore::new(&dir.path().to_string_lossy(), FlatKvConfig::default());
                store.load_version(0).unwrap();
                (dir, store)
            },
            |(_dir, mut store)| {
                // Simulate one block: 10 storage writes
                let pairs: Vec<KvPair> = (0..10)
                    .map(|i| {
                        let mut key = vec![0x03u8]; // STATE_KEY_PREFIX
                        key.extend_from_slice(&[i as u8; 20]); // addr
                        key.extend_from_slice(&[0u8; 32]); // slot
                        KvPair { delete: false, key, value: vec![0u8; 32] }
                    })
                    .collect();
                let cs = vec![NamedChangeSet {
                    name: "evm".into(),
                    changeset: Some(ChangeSet { pairs }),
                }];
                store.apply_change_sets(&cs).unwrap();
                store.commit().unwrap();
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_flatkv_snapshot_checkpoint(c: &mut Criterion) {
    // Pre-build a store with 100 blocks of 10 storage keys each
    let dir = tempdir().unwrap();
    let mut store = CommitStore::new(
        &dir.path().to_string_lossy(),
        FlatKvConfig { snapshot_interval: 0, ..Default::default() },
    );
    store.load_version(0).unwrap();

    for block in 1..=100u32 {
        let pairs: Vec<KvPair> = (0..10)
            .map(|i| {
                let mut key = vec![0x03u8]; // STATE_KEY_PREFIX
                key.extend_from_slice(&[(block.wrapping_mul(10).wrapping_add(i)) as u8; 20]); // addr
                key.extend_from_slice(&[0u8; 32]); // slot
                KvPair { delete: false, key, value: vec![0u8; 32] }
            })
            .collect();
        let cs = vec![NamedChangeSet { name: "evm".into(), changeset: Some(ChangeSet { pairs }) }];
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();
    }

    c.bench_function("flatkv_snapshot_checkpoint", |b| {
        b.iter(|| {
            store.write_snapshot().unwrap();
        });
    });

    store.close().unwrap();
}

criterion_group!(
    benches,
    bench_lthash_serial,
    bench_lthash_parallel,
    bench_flatkv_apply_commit,
    bench_flatkv_snapshot_checkpoint
);
criterion_main!(benches);
