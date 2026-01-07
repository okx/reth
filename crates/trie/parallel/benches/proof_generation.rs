//! Benchmark for measuring MDBX read latency during state root calculation.
//!
//! This measures the time to calculate state roots, which involves:
//! - Reading trie nodes from MDBX
//! - Generating proofs internally
//! - Walking the trie structure
//!
//! Run with: cargo bench --package reth-trie-parallel --bench proof_generation

#![allow(missing_docs)]

use alloy_primitives::{B256, U256};
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode,
};
use proptest::{prelude::*, strategy::ValueTree, test_runner::TestRunner};
use proptest_arbitrary_interop::arb;
use reth_primitives_traits::Account;
use reth_provider::{test_utils::create_test_provider_factory, StateWriter, TrieWriter};
use reth_trie::{
    hashed_cursor::HashedPostStateCursorFactory, HashedPostState, HashedStorage, StateRoot,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseStateRoot};
use std::{collections::HashMap, time::Instant};

/// Generate test data with accounts and storage
fn generate_test_data(num_accounts: usize, storage_slots: usize) -> HashedPostState {
    let mut runner = TestRunner::deterministic();

    use proptest::{collection::hash_map, prelude::any};
    let db_state = hash_map(
        any::<B256>(),
        (
            arb::<Account>().prop_filter("non empty account", |a| !a.is_empty()),
            hash_map(
                any::<B256>(),
                any::<U256>().prop_filter("non zero value", |v| !v.is_zero()),
                storage_slots,
            ),
        ),
        num_accounts,
    )
    .new_tree(&mut runner)
    .unwrap()
    .current();

    HashedPostState::default()
        .with_accounts(db_state.iter().map(|(address, (account, _))| (*address, Some(*account))))
        .with_storages(
            db_state
                .into_iter()
                .map(|(address, (_, storage))| (address, HashedStorage::from_iter(false, storage))),
        )
}

/// Benchmark state root calculation with varying complexity
/// This internally performs MDBX trie node reads
fn bench_state_root_mdbx_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_root_mdbx_timing");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);

    // Test different scales to measure MDBX read performance
    let scenarios = vec![
        ("small_100_accounts", 100, 10),
        ("medium_500_accounts", 500, 10),
        ("large_1000_accounts", 1000, 20),
    ];

    for (name, num_accounts, storage_slots) in scenarios {
        let (db_state, updated_state) = {
            let full_state = generate_test_data(num_accounts, storage_slots);
            let keys: Vec<_> = full_state.accounts.keys().copied().collect();
            let update_keys: Vec<_> = keys.iter().take(num_accounts / 2).copied().collect();

            let db_state = full_state.clone();
            let mut updated_state = HashedPostState::default();

            for key in update_keys {
                if let Some(storage) = db_state.storages.get(&key) {
                    updated_state.storages.insert(key, storage.clone());
                }
            }

            (db_state, updated_state)
        };

        let factory = create_test_provider_factory();
        {
            let provider_rw = factory.provider_rw().unwrap();
            provider_rw.write_hashed_state(&db_state.into_sorted()).unwrap();
            let (_, updates) =
                StateRoot::from_tx(provider_rw.tx_ref()).root_with_updates().unwrap();
            provider_rw.write_trie_updates(updates).unwrap();
            provider_rw.commit().unwrap();
        }

        group.bench_function(BenchmarkId::new("increm ental_root", name), |b| {
            b.iter_with_setup(
                || {
                    let sorted_state = updated_state.clone().into_sorted();
                    let prefix_sets = updated_state.construct_prefix_sets().freeze();
                    let provider = factory.provider().unwrap();
                    (provider, sorted_state, prefix_sets)
                },
                |(provider, sorted_state, prefix_sets)| {
                    let start = Instant::now();

                    // This internally performs MDBX trie node reads
                    let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                        DatabaseHashedCursorFactory::new(provider.tx_ref()),
                        &sorted_state,
                    );
                    let _root = StateRoot::from_tx(provider.tx_ref())
                        .with_hashed_cursor_factory(hashed_cursor_factory)
                        .with_prefix_sets(prefix_sets)
                        .root()
                        .expect("failed to compute root");

                    let elapsed = start.elapsed();
                    black_box(elapsed)
                },
            )
        });

        // Also bench full root (more MDBX reads)
        group.bench_function(BenchmarkId::new("full_root", name), |b| {
            b.iter_with_setup(
                || factory.provider().unwrap(),
                |provider| {
                    let start = Instant::now();

                    let _root =
                        StateRoot::from_tx(provider.tx_ref()).root().expect("failed to compute root");

                    let elapsed = start.elapsed();
                    black_box(elapsed)
                },
            )
        });
    }

    group.finish();
}

/// Benchmark with different update sizes to see MDBX read scaling
fn bench_update_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("update_size_scaling");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);

    let base_state = generate_test_data(1000, 10);
    let factory = create_test_provider_factory();
    {
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.write_hashed_state(&base_state.clone().into_sorted()).unwrap();
        let (_, updates) = StateRoot::from_tx(provider_rw.tx_ref()).root_with_updates().unwrap();
        provider_rw.write_trie_updates(updates).unwrap();
        provider_rw.commit().unwrap();
    }

    // Test different update sizes
    for update_size in [10, 50, 100, 200] {
        let keys: Vec<_> = base_state.accounts.keys().copied().collect();
        let update_keys: Vec<_> = keys.iter().take(update_size).copied().collect();

        let mut updated_state = HashedPostState::default();
        for key in update_keys {
            if let Some(storage) = base_state.storages.get(&key) {
                updated_state.storages.insert(key, storage.clone());
            }
        }

        group.bench_function(BenchmarkId::new("accounts_updated", update_size), |b| {
            b.iter_with_setup(
                || {
                    let sorted_state = updated_state.clone().into_sorted();
                    let prefix_sets = updated_state.construct_prefix_sets().freeze();
                    let provider = factory.provider().unwrap();
                    (provider, sorted_state, prefix_sets)
                },
                |(provider, sorted_state, prefix_sets)| {
                    let start = Instant::now();

                    let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                        DatabaseHashedCursorFactory::new(provider.tx_ref()),
                        &sorted_state,
                    );
                    let _root = StateRoot::from_tx(provider.tx_ref())
                        .with_hashed_cursor_factory(hashed_cursor_factory)
                        .with_prefix_sets(prefix_sets)
                        .root()
                        .expect("failed to compute root");

                    black_box(start.elapsed())
                },
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_state_root_mdbx_reads, bench_update_sizes,);
criterion_main!(benches);
