use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use seidb_sc::memiavl::tree::Tree;
use tempfile::tempdir;

fn bench_tree_insert(c: &mut Criterion) {
    c.bench_function("memiavl_insert_1k_random", |b| {
        b.iter_batched(
            || Tree::new_empty(0, 0),
            |mut tree| {
                for i in 0..1000u32 {
                    let key = format!("key_{:08x}", i.wrapping_mul(2654435761)); // scramble
                    tree.set(key.as_bytes(), b"value_data_here");
                }
                tree.save_version(true).unwrap();
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_tree_write_snapshot(c: &mut Criterion) {
    // Pre-build a 10K tree
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..10000u32 {
        let key = format!("key_{:08x}", i.wrapping_mul(2654435761));
        tree.set(key.as_bytes(), &[0u8; 64]);
    }
    tree.save_version(true).unwrap();

    c.bench_function("memiavl_snapshot_10k", |b| {
        b.iter_batched(
            || tempdir().unwrap(),
            |dir| {
                tree.write_snapshot(dir.path()).unwrap();
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_tree_get(c: &mut Criterion) {
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..10000u32 {
        let key = format!("key_{:08x}", i.wrapping_mul(2654435761));
        tree.set(key.as_bytes(), &[0u8; 64]);
    }
    tree.save_version(true).unwrap();

    c.bench_function("memiavl_get_10k", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = i.wrapping_add(7919) % 10000;
            let key = format!("key_{:08x}", i.wrapping_mul(2654435761));
            tree.get(key.as_bytes())
        });
    });
}

fn bench_tree_remove(c: &mut Criterion) {
    c.bench_function("memiavl_remove_500_from_1k", |b| {
        b.iter_batched(
            || {
                let mut tree = Tree::new_empty(0, 0);
                for i in 0..1000u32 {
                    tree.set(&i.to_be_bytes(), &[0u8; 32]);
                }
                tree.save_version(false).unwrap();
                tree
            },
            |mut tree| {
                for i in (0..1000u32).step_by(2) {
                    tree.remove(&i.to_be_bytes());
                }
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_tree_get_proof(c: &mut Criterion) {
    // Build a tree with 1000 keys
    let mut tree = Tree::new_empty(0, 0);
    for i in 0..1000u32 {
        let key = format!("key_{:08x}", i.wrapping_mul(2654435761));
        tree.set(key.as_bytes(), &[0u8; 64]);
    }
    tree.save_version(true).unwrap();

    // Pre-compute keys for benchmark iteration
    let keys: Vec<String> =
        (0..1000u32).map(|i| format!("key_{:08x}", i.wrapping_mul(2654435761))).collect();

    c.bench_function("memiavl_get_proof_1k", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 1) % keys.len();
            tree.get_membership_proof(keys[i].as_bytes())
        });
    });
}

criterion_group!(
    benches,
    bench_tree_insert,
    bench_tree_write_snapshot,
    bench_tree_get,
    bench_tree_remove,
    bench_tree_get_proof
);
criterion_main!(benches);
