//! Benchmark: mpt-db MPT vs reth MPT+MDBX.
//!
//! Compares two MPT implementations using identical deterministic inputs.
//! Both systems use chunked pre-population (incremental state building),
//! matching real blockchain behavior where state accumulates block by block.
//! Pre-population is excluded from measured time.
//!
//! B4.1: fresh-state one-shot (100 accounts, 10 slots each)
//! B4.2: pre-populated DB + single block (1K pre-pop, 200 updated)
//! B4.3: 10 blocks incremental (1K pre-pop, 200 updates/block)
//! B4.4: large-scale (200K pre-pop + 2K updates/block, 10 blocks) — BENCH_LARGE=1
//! B4.5: near-production (1M pre-pop + 5K updates/block, 10 blocks) — BENCH_LARGE=1
//! B4.6: storage-heavy (1M pre-pop 30 slots + 10K updates/block, 10 blocks) — BENCH_LARGE=1

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use reth_provider::{test_utils::create_test_provider_factory, StateWriter, TrieWriter};
use reth_trie::{updates::TrieUpdates, HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;

use mptdb_sc::mpt::{MptCommitStore, MptCommitter};

const PREPOP_CHUNK_SIZE: usize = 10_000;

// ---------------------------------------------------------------------------
// Data generation (deterministic, shared by both lanes)
// ---------------------------------------------------------------------------

/// Generate `num` random addresses.
fn generate_addresses(num: usize, rng: &mut StdRng) -> Vec<Address> {
    let mut addresses = Vec::with_capacity(num);
    let mut addr_buf = [0u8; 20];
    for _ in 0..num {
        rng.fill_bytes(&mut addr_buf);
        addresses.push(Address::from(addr_buf));
    }
    addresses
}

/// Generate a BundleState for a chunk of addresses.
fn generate_account_chunk(
    addresses: &[Address],
    start_index: usize,
    slots_per: usize,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    for (offset, &addr) in addresses.iter().enumerate() {
        let i = start_index + offset;
        let info = AccountInfo {
            nonce: i as u64,
            balance: U256::from(1_000_000 * (i + 1)),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        };
        let mut storage = StorageWithOriginalValues::default();
        for j in 0..slots_per {
            let mut slot_bytes = [0u8; 32];
            slot_bytes[24..32].copy_from_slice(&(j as u64).to_be_bytes());
            storage.insert(
                B256::from(slot_bytes).into(),
                StorageSlot::new_changed(U256::ZERO, U256::from(j + 1)),
            );
        }
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info),
                original_info: None,
                status: AccountStatus::Changed,
                storage,
            },
        );
    }
    revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

/// Generate one block of updates: pick `count` addresses, bump nonce/balance/storage.
fn generate_updates(
    addresses: &[Address],
    count: usize,
    slots_per: usize,
    block_idx: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    let indices: Vec<usize> = (0..count).map(|_| rng.random_range(0..addresses.len())).collect();

    for (i, &idx) in indices.iter().enumerate() {
        let addr = addresses[idx];
        let nonce = (block_idx * count + i) as u64;
        let balance = U256::from(1_000_000 * (block_idx * count + i + 1));

        let info =
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, account_id: None, code: None };

        let mut storage = StorageWithOriginalValues::default();
        for j in 0..slots_per {
            let mut slot_bytes = [0u8; 32];
            slot_bytes[24..32].copy_from_slice(&(j as u64).to_be_bytes());
            storage.insert(
                B256::from(slot_bytes).into(),
                StorageSlot::new_changed(U256::ZERO, U256::from((block_idx + j) as u128 + 1)),
            );
        }

        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info),
                original_info: None,
                status: AccountStatus::Changed,
                storage,
            },
        );
    }

    revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

// ---------------------------------------------------------------------------
// Unified lane functions — both use chunked pre-pop
// ---------------------------------------------------------------------------

/// Run reth lane: chunked pre-pop (not timed) + process blocks (timed).
///
/// Pre-pop uses incremental chunks matching real blockchain state accumulation:
/// each chunk writes hashed state, computes trie root, and commits.
fn run_reth_lane(
    addresses: &[Address],
    slots_per: usize,
    block_bundles: &[revm_database::BundleState],
) -> Duration {
    let factory = create_test_provider_factory();

    // Pre-populate in chunks (not timed)
    for (chunk_idx, chunk) in addresses.chunks(PREPOP_CHUNK_SIZE).enumerate() {
        let start_index = chunk_idx * PREPOP_CHUNK_SIZE;
        let bundle = generate_account_chunk(chunk, start_index, slots_per);
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let sorted = hashed.into_sorted();
        let rw = factory.provider_rw().unwrap();
        rw.write_hashed_state(&sorted).unwrap();
        let (_, updates): (B256, TrieUpdates) =
            StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
        rw.write_trie_updates(updates).unwrap();
        rw.commit().unwrap();
    }

    // Process blocks (timed)
    let start = Instant::now();
    for bundle in block_bundles {
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let sorted = hashed.into_sorted();
        let rw = factory.provider_rw().unwrap();
        let (_, updates) = StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted).unwrap();
        rw.write_hashed_state(&sorted).unwrap();
        rw.write_trie_updates(updates).unwrap();
        rw.commit().unwrap();
    }
    start.elapsed()
}

/// Run mpt-db lane: chunked pre-pop (not timed) + process blocks (timed).
///
/// Pre-pop uses incremental apply_bundle_state + commit per chunk,
/// matching real blockchain state accumulation.  wal_first mode enabled.
fn run_mptdb_lane(
    addresses: &[Address],
    slots_per: usize,
    block_bundles: &[revm_database::BundleState],
) -> Duration {
    let dir = TempDir::new().unwrap();
    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.wal_first_commit = true;
    config.checkpoint_max_account_trie_nodes = 0;
    let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

    // Pre-populate in chunks (not timed)
    for (chunk_idx, chunk) in addresses.chunks(PREPOP_CHUNK_SIZE).enumerate() {
        let start_index = chunk_idx * PREPOP_CHUNK_SIZE;
        let bundle = generate_account_chunk(chunk, start_index, slots_per);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
    }

    // Process blocks (timed)
    let start = Instant::now();
    for bundle in block_bundles {
        store.apply_bundle_state(bundle).unwrap();
        store.commit().unwrap();
    }
    let elapsed = start.elapsed();
    let _ = store.close();
    elapsed
}

// ---------------------------------------------------------------------------
// Benchmark cases
// ---------------------------------------------------------------------------

/// B4.1: Fresh-state one-shot — 100 accounts, 10 slots each, compute root from scratch.
fn bench_b4_1_fresh_state(c: &mut Criterion) {
    let mut group = c.benchmark_group("B4.1 fresh-state one-shot");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let mut rng = StdRng::seed_from_u64(4100);
    let addrs = generate_addresses(100, &mut rng);

    let mut rng_block = StdRng::seed_from_u64(4101);
    let block = generate_account_chunk(&addrs, 0, 10);

    // For fresh-state, no pre-pop — the block IS the first state.
    let empty: Vec<Address> = vec![];

    group.bench_function(BenchmarkId::new("reth_mpt", "100accts_10slots"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&empty, 10, &[block.clone()]);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "100accts_10slots"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&empty, 10, &[block.clone()]);
            }
            total
        })
    });

    group.finish();
}

/// B4.2: Pre-populated DB + single block — 1K pre-pop, 200 updated in one block.
fn bench_b4_2_prepop_single_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("B4.2 prepop + single block");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let mut rng = StdRng::seed_from_u64(4200);
    let addrs = generate_addresses(1_000, &mut rng);

    let mut rng_block = StdRng::seed_from_u64(4201);
    let block = generate_updates(&addrs, 200, 10, 0, &mut rng_block);

    group.bench_function(BenchmarkId::new("reth_mpt", "1K_pre_200_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&addrs, 10, &[block.clone()]);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "1K_pre_200_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&addrs, 10, &[block.clone()]);
            }
            total
        })
    });

    group.finish();
}

/// B4.3: 10 blocks incremental — 1K pre-pop, 200 updates per block.
fn bench_b4_3_incremental_blocks(c: &mut Criterion) {
    let mut group = c.benchmark_group("B4.3 incremental 10 blocks");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let mut rng = StdRng::seed_from_u64(4300);
    let addrs = generate_addresses(1_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4301);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 200, 10, i, &mut rng_blocks)).collect();

    group.bench_function(BenchmarkId::new("reth_mpt", "1K_pre_10x200_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "1K_pre_10x200_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.finish();
}

/// B4.4: Large-scale — 200K pre-pop + 2K updates/block, 10 blocks.
/// Gated by BENCH_LARGE=1 environment variable.
fn bench_b4_4_large_scale(c: &mut Criterion) {
    if std::env::var("BENCH_LARGE").is_err() {
        eprintln!("Skipping B4.4 (set BENCH_LARGE=1 to run)");
        return;
    }

    let mut group = c.benchmark_group("B4.4 large-scale");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let mut rng = StdRng::seed_from_u64(4400);
    let addrs = generate_addresses(200_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4401);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 2_000, 10, i, &mut rng_blocks)).collect();

    group.bench_function(BenchmarkId::new("reth_mpt", "200K_pre_10x2K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "200K_pre_10x2K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.finish();
}

/// B4.5: Near-production — 1M pre-pop (10 slots each) + 5K updates/block, 10 blocks.
/// Gated by BENCH_LARGE=1 environment variable.
fn bench_b4_5_near_production(c: &mut Criterion) {
    if std::env::var("BENCH_LARGE").is_err() {
        eprintln!("Skipping B4.5 (set BENCH_LARGE=1 to run)");
        return;
    }

    let mut group = c.benchmark_group("B4.5 near-production");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(120));

    let mut rng = StdRng::seed_from_u64(4500);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4501);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 5_000, 10, i, &mut rng_blocks)).collect();

    group.bench_function(BenchmarkId::new("reth_mpt", "1M_pre_10x5K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "1M_pre_10x5K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&addrs, 10, &blocks);
            }
            total
        })
    });

    group.finish();
}

/// B4.6: Storage-heavy large-scale — 1M pre-pop (30 slots each) + 10K updates/block, 10 blocks.
/// Gated by BENCH_LARGE=1 environment variable.
fn bench_b4_6_storage_heavy_large(c: &mut Criterion) {
    if std::env::var("BENCH_LARGE").is_err() {
        eprintln!("Skipping B4.6 (set BENCH_LARGE=1 to run)");
        return;
    }

    let mut group = c.benchmark_group("B4.6 storage-heavy large");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(180));

    let mut rng = StdRng::seed_from_u64(4600);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4601);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 10_000, 30, i, &mut rng_blocks)).collect();

    group.bench_function(BenchmarkId::new("reth_mpt", "1M_30s_10x10K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_reth_lane(&addrs, 30, &blocks);
            }
            total
        })
    });

    group.bench_function(BenchmarkId::new("mptdb_mpt", "1M_30s_10x10K_upd"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += run_mptdb_lane(&addrs, 30, &blocks);
            }
            total
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_b4_1_fresh_state,
    bench_b4_2_prepop_single_block,
    bench_b4_3_incremental_blocks,
    bench_b4_4_large_scale,
    bench_b4_5_near_production,
    bench_b4_6_storage_heavy_large
);
criterion_main!(benches);
