use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mptdb_common::config::StateStoreConfig;
use mptdb_engine::mvcc::db::MvccDatabase;
use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
use mptdb_traits::ss::StateStore;
use tempfile::tempdir;

fn bench_mvcc_sequential_write(c: &mut Criterion) {
    c.bench_function("mvcc_sequential_write_1k", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().unwrap();
                let config = StateStoreConfig {
                    db_directory: dir.path().to_string_lossy().to_string(),
                    ..Default::default()
                };
                let db = MvccDatabase::open_db(&config).unwrap();
                (dir, db)
            },
            |(dir, db)| {
                for v in 1..=1000 {
                    let cs = vec![NamedChangeSet {
                        name: "bench".into(),
                        changeset: Some(ChangeSet {
                            pairs: vec![KvPair {
                                delete: false,
                                key: format!("key_{v:06}").into_bytes(),
                                value: vec![0u8; 100],
                            }],
                        }),
                    }];
                    db.apply_changeset_sync(v, &cs).unwrap();
                }
                drop(db);
                drop(dir);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_mvcc_random_read(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let config = StateStoreConfig {
        db_directory: dir.path().to_string_lossy().to_string(),
        ..Default::default()
    };
    let db = MvccDatabase::open_db(&config).unwrap();
    // Setup: write 10K keys across 100 versions
    for v in 1..=100 {
        let pairs: Vec<KvPair> = (0..100)
            .map(|i| KvPair {
                delete: false,
                key: format!("key_{:06}", v * 100 + i).into_bytes(),
                value: vec![0u8; 100],
            })
            .collect();
        let cs =
            vec![NamedChangeSet { name: "bench".into(), changeset: Some(ChangeSet { pairs }) }];
        db.apply_changeset_sync(v, &cs).unwrap();
    }

    c.bench_function("mvcc_random_read", |b| {
        let mut i = 0u64;
        b.iter(|| {
            i = (i + 7919) % 10000; // pseudo-random
            let _ = db.get("bench", 100, &format!("key_{i:06}").into_bytes());
        });
    });
}

fn bench_mvcc_iterator_forward(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let config = StateStoreConfig {
        db_directory: dir.path().to_string_lossy().to_string(),
        ..Default::default()
    };
    let db = MvccDatabase::open_db(&config).unwrap();
    for v in 1..=10 {
        let pairs: Vec<KvPair> = (0..1000)
            .map(|i| KvPair {
                delete: false,
                key: format!("key_{:06}", v * 1000 + i).into_bytes(),
                value: vec![0u8; 64],
            })
            .collect();
        let cs =
            vec![NamedChangeSet { name: "bench".into(), changeset: Some(ChangeSet { pairs }) }];
        db.apply_changeset_sync(v, &cs).unwrap();
    }

    c.bench_function("mvcc_iterator_forward_10k", |b| {
        b.iter(|| {
            let mut iter = db.iterator("bench", 10, &[], &[]).unwrap();
            let mut count = 0;
            while iter.valid() {
                count += 1;
                iter.next();
            }
            count
        });
    });
}

fn bench_mvcc_prune(c: &mut Criterion) {
    c.bench_function("mvcc_prune_100_versions", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().unwrap();
                let config = StateStoreConfig {
                    db_directory: dir.path().to_string_lossy().to_string(),
                    keep_last_version: true,
                    ..Default::default()
                };
                let db = MvccDatabase::open_db(&config).unwrap();
                for v in 1..=100 {
                    let cs = vec![NamedChangeSet {
                        name: "bench".into(),
                        changeset: Some(ChangeSet {
                            pairs: vec![KvPair {
                                delete: false,
                                key: b"prune_key".to_vec(),
                                value: format!("val_{v}").into_bytes(),
                            }],
                        }),
                    }];
                    db.apply_changeset_sync(v, &cs).unwrap();
                }
                (dir, db)
            },
            |(_dir, db)| {
                db.prune(90).unwrap();
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group!(
    benches,
    bench_mvcc_sequential_write,
    bench_mvcc_random_read,
    bench_mvcc_iterator_forward,
    bench_mvcc_prune
);
criterion_main!(benches);
