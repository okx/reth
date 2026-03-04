//! Lightweight benchmark: compare SALT backends (MdbxSaltStore, FlatFileStore, RocksSaltStore).
//!
//! Same workload as block_io.rs Random scenario but runs faster (fewer criterion samples).

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rayon::{prelude::*, ThreadPoolBuilder};
use salt::{EphemeralSaltState, StateRoot as SaltStateRoot};
use xlayer_salt::{
    async_rocks_store::AsyncRocksStore, convert::bundle_state_to_plain_kv,
    flat_store::FlatFileStore, mdbx_store::MdbxSaltStore, rocks_store::RocksSaltStore,
};

const PRE_POP_ACCOUNTS: usize = 200_000;
const NUM_BLOCKS: usize = 10;
const ACCOUNTS_PER_BLOCK: usize = 2000;
const SLOTS_PER_ACCOUNT: usize = 10;
const SALT_NUM_THREADS: usize = 32;

// ---------------------------------------------------------------------------
// Data generation (same as block_io.rs)
// ---------------------------------------------------------------------------

fn generate_bundle_state_random(
    num_accounts: usize,
    slots_per_account: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    let mut addr_buf = [0u8; 20];

    for i in 0..num_accounts {
        rng.fill_bytes(&mut addr_buf);
        let addr = Address::from(addr_buf);
        let info = AccountInfo {
            nonce: i as u64,
            balance: U256::from(1_000_000 * (i + 1)),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        };
        let mut storage = StorageWithOriginalValues::default();
        for j in 0..slots_per_account {
            let mut slot_bytes = [0u8; 32];
            slot_bytes[24..32].copy_from_slice(&(j as u64).to_be_bytes());
            let slot_key = B256::from(slot_bytes);
            storage
                .insert(slot_key.into(), StorageSlot::new_changed(U256::ZERO, U256::from(j + 1)));
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

fn generate_block_updates_from_addresses(
    addresses: &[Address],
    slots_per_account: usize,
    block_index: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    let indices: Vec<usize> =
        (0..ACCOUNTS_PER_BLOCK).map(|_| rng.random_range(0..addresses.len())).collect();

    for (i, &idx) in indices.iter().enumerate() {
        let addr = addresses[idx];
        let nonce = (block_index * ACCOUNTS_PER_BLOCK + i) as u64;
        let balance = U256::from(1_000_000 * (block_index * ACCOUNTS_PER_BLOCK + i + 1));
        let info =
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, account_id: None, code: None };
        let mut storage = StorageWithOriginalValues::default();
        for j in 0..slots_per_account {
            let mut slot_bytes = [0u8; 32];
            slot_bytes[24..32].copy_from_slice(&(j as u64).to_be_bytes());
            let slot_key = B256::from(slot_bytes);
            storage.insert(
                slot_key.into(),
                StorageSlot::new_changed(U256::ZERO, U256::from((block_index + j) as u128 + 1)),
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

fn bundle_state_changes(bundle: &revm_database::BundleState) -> usize {
    bundle.state().iter().map(|(_, a)| 1 + a.storage.len()).sum()
}

// ---------------------------------------------------------------------------
// Per-block stats (simplified)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BlockStats {
    wall_time: Duration,
    state_prep_time: Duration,
    state_delta_time: Duration,
    root_compute_time: Duration,
    disk_io_time: Duration,
    state_entries: usize,
    trie_entries: usize,
}

fn print_stats(label: &str, stats: &[BlockStats]) {
    let n = stats.len();
    if n == 0 {
        return;
    }
    let avg = |f: fn(&BlockStats) -> Duration| -> Duration {
        stats.iter().map(f).sum::<Duration>() / n as u32
    };
    let avg_usize = |f: fn(&BlockStats) -> usize| -> f64 {
        stats.iter().map(f).sum::<usize>() as f64 / n as f64
    };
    eprintln!("─── {} ───", label);
    eprintln!(
        "  {} blocks avg: {:.2?}  (prep {:.2?}  delta {:.2?}  root {:.2?}  io {:.2?})",
        n,
        avg(|s| s.wall_time),
        avg(|s| s.state_prep_time),
        avg(|s| s.state_delta_time),
        avg(|s| s.root_compute_time),
        avg(|s| s.disk_io_time),
    );
    eprintln!(
        "  writes/blk: state {:.0}, trie {:.0}",
        avg_usize(|s| s.state_entries),
        avg_usize(|s| s.trie_entries),
    );
    eprintln!();
}

// ---------------------------------------------------------------------------
// MDBX backend
// ---------------------------------------------------------------------------

fn reset_mdbx_pre_pop(
    store: &MdbxSaltStore,
    pre_pop: &revm_database::BundleState,
    pool: &rayon::ThreadPool,
) {
    let kvs = bundle_state_to_plain_kv(pre_pop);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_min_par_batch_size(4).with_deferred_levels(2);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state(state_updates).unwrap();
    store.update_trie(trie_updates).unwrap();
}

fn run_mdbx_blocks(
    store: &MdbxSaltStore,
    root: &mut SaltStateRoot<'_, MdbxSaltStore>,
    pool: &rayon::ThreadPool,
    block_bundles: &[revm_database::BundleState],
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();
    for bundle in block_bundles {
        let block_start = Instant::now();
        let prep_start = Instant::now();
        let kvs = bundle_state_to_plain_kv(bundle);
        let prep_time = prep_start.elapsed();

        let delta_start = Instant::now();
        let mut eph = EphemeralSaltState::new(store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        let delta_time = delta_start.elapsed();

        let root_start = Instant::now();
        let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
        let root_time = root_start.elapsed();

        let io_start = Instant::now();
        let state_entries = state_updates.data.len();
        let trie_entries = trie_updates.len();
        store.update_state(state_updates).unwrap();
        store.update_trie(trie_updates).unwrap();
        let io_time = io_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: root_time,
            disk_io_time: io_time,
            state_entries,
            trie_entries,
        });
    }
    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// FlatFile backend
// ---------------------------------------------------------------------------

fn reset_flat_pre_pop(
    store: &FlatFileStore,
    pre_pop: &revm_database::BundleState,
    pool: &rayon::ThreadPool,
) {
    let kvs = bundle_state_to_plain_kv(pre_pop);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_min_par_batch_size(4).with_deferred_levels(2);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state(state_updates).unwrap();
    store.update_trie(trie_updates).unwrap();
}

fn run_flat_blocks(
    store: &FlatFileStore,
    root: &mut SaltStateRoot<'_, FlatFileStore>,
    pool: &rayon::ThreadPool,
    block_bundles: &[revm_database::BundleState],
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();
    for bundle in block_bundles {
        let block_start = Instant::now();
        let prep_start = Instant::now();
        let kvs = bundle_state_to_plain_kv(bundle);
        let prep_time = prep_start.elapsed();

        let delta_start = Instant::now();
        let mut eph = EphemeralSaltState::new(store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        let delta_time = delta_start.elapsed();

        let root_start = Instant::now();
        let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
        let root_time = root_start.elapsed();

        let io_start = Instant::now();
        let state_entries = state_updates.data.len();
        let trie_entries = trie_updates.len();
        store.update_state(state_updates).unwrap();
        store.update_trie(trie_updates).unwrap();
        let io_time = io_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: root_time,
            disk_io_time: io_time,
            state_entries,
            trie_entries,
        });
    }
    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// RocksDB backend (for comparison baseline)
// ---------------------------------------------------------------------------

fn reset_rocks_pre_pop(
    store: &RocksSaltStore,
    pre_pop: &revm_database::BundleState,
    pool: &rayon::ThreadPool,
) {
    let kvs = bundle_state_to_plain_kv(pre_pop);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_min_par_batch_size(4).with_deferred_levels(2);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state_and_trie(state_updates, trie_updates).unwrap();
}

fn run_rocks_blocks(
    store: &RocksSaltStore,
    root: &mut SaltStateRoot<'_, RocksSaltStore>,
    pool: &rayon::ThreadPool,
    block_bundles: &[revm_database::BundleState],
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();
    for bundle in block_bundles {
        let block_start = Instant::now();
        let prep_start = Instant::now();
        let kvs = bundle_state_to_plain_kv(bundle);
        let prep_time = prep_start.elapsed();

        let delta_start = Instant::now();
        let mut eph = EphemeralSaltState::new(store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        let delta_time = delta_start.elapsed();

        let root_start = Instant::now();
        let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
        let root_time = root_start.elapsed();

        let io_start = Instant::now();
        let state_entries = state_updates.data.len();
        let trie_entries = trie_updates.len();
        store.update_state_and_trie(state_updates, trie_updates).unwrap();
        let io_time = io_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: root_time,
            disk_io_time: io_time,
            state_entries,
            trie_entries,
        });
    }
    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// AsyncRocksDB backend (in-memory reads + async RocksDB persistence)
// ---------------------------------------------------------------------------

fn reset_async_rocks_pre_pop(
    store: &AsyncRocksStore,
    pre_pop: &revm_database::BundleState,
    pool: &rayon::ThreadPool,
) {
    let kvs = bundle_state_to_plain_kv(pre_pop);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_min_par_batch_size(4).with_deferred_levels(2);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state_and_trie(state_updates, trie_updates).unwrap();
    // Drain the massive pre-pop write so benchmark blocks start with an idle writer.
    store.wait_for_idle();
}

fn run_async_rocks_blocks(
    store: &AsyncRocksStore,
    root: &mut SaltStateRoot<'_, AsyncRocksStore>,
    pool: &rayon::ThreadPool,
    block_bundles: &[revm_database::BundleState],
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();
    for bundle in block_bundles {
        let block_start = Instant::now();
        let prep_start = Instant::now();
        let kvs = bundle_state_to_plain_kv(bundle);
        let prep_time = prep_start.elapsed();

        // Parallel delta: partition KVs by SALT bucket_id, process each partition
        // with its own EphemeralSaltState on a rayon thread, merge results.
        // Safe because different bucket_ids → non-overlapping SaltKeys.
        let delta_start = Instant::now();
        let state_updates = {
            let session = store.read_session();

            // Group KVs by SALT bucket_id (just references, no copies).
            let mut groups: std::collections::HashMap<u32, Vec<(&Vec<u8>, &Option<Vec<u8>>)>> =
                std::collections::HashMap::new();
            for (key, val) in &kvs {
                let bid = salt::hasher::bucket_id(key);
                groups.entry(bid).or_default().push((key, val));
            }

            // Distribute groups into N partitions (round-robin by bucket).
            let num_partitions = pool.current_num_threads().max(1);
            let mut partitions: Vec<Vec<(&Vec<u8>, &Option<Vec<u8>>)>> =
                (0..num_partitions).map(|_| Vec::new()).collect();
            for (i, (_, group_kvs)) in groups.into_iter().enumerate() {
                partitions[i % num_partitions].extend(group_kvs);
            }

            // Process partitions in parallel — each gets its own EphemeralSaltState.
            let results: Vec<salt::StateUpdates> = pool.install(|| {
                partitions
                    .into_par_iter()
                    .filter_map(|partition| {
                        if partition.is_empty() {
                            return None;
                        }
                        let mut eph = EphemeralSaltState::new(&session);
                        Some(eph.update_fin(partition.into_iter()).unwrap())
                    })
                    .collect()
            });

            // Merge non-overlapping StateUpdates (different bucket_ids → disjoint keys).
            let mut merged = salt::StateUpdates::default();
            for updates in results {
                merged.data.extend(updates.data);
            }
            Arc::new(merged)
        };
        let delta_time = delta_start.elapsed();

        // Dispatch Arc to background writer (~ns).
        store.dispatch_state_to_bg(Arc::clone(&state_updates)).unwrap();

        // Overlap in-memory state update with root computation.
        let overlap_start = Instant::now();
        let ((_root_hash, trie_updates), ws) = pool.install(|| {
            rayon::join(
                || root.update_fin(&state_updates).unwrap(),
                || store.apply_state_in_memory(&state_updates),
            )
        });
        let overlap_time = overlap_start.elapsed();

        // Trie dispatch after root.
        let trie_io_start = Instant::now();
        let trie_entries = store.update_trie(trie_updates).unwrap();
        let trie_dispatch_time = trie_io_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: overlap_time,
            disk_io_time: ws.persist_duration + trie_dispatch_time,
            state_entries: ws.entries,
            trie_entries,
        });
    }
    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// Benchmark
// ---------------------------------------------------------------------------

fn bench_store_compare(c: &mut Criterion) {
    let mut group = c.benchmark_group("Store comparison");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let pool = ThreadPoolBuilder::new().num_threads(SALT_NUM_THREADS).build().unwrap();

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");
    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);

    let mut rng = StdRng::seed_from_u64(42);
    let pre_pop = generate_bundle_state_random(PRE_POP_ACCOUNTS, SLOTS_PER_ACCOUNT, &mut rng);
    let pre_pop_addresses: Vec<Address> = pre_pop.state().keys().copied().collect();

    let mut rng_blocks = StdRng::seed_from_u64(43);
    let block_bundles: Vec<_> = (0..NUM_BLOCKS)
        .map(|i| {
            generate_block_updates_from_addresses(
                &pre_pop_addresses,
                SLOTS_PER_ACCOUNT,
                i,
                &mut rng_blocks,
            )
        })
        .collect();

    eprintln!(
        "Setup: {} accounts, {} state changes/block",
        PRE_POP_ACCOUNTS, state_changes_per_block
    );

    // ---- MDBX ----
    group.bench_function(BenchmarkId::new("mdbx", &label), |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::TempDir::new().unwrap();
            let store = MdbxSaltStore::new(dir.path()).unwrap();
            reset_mdbx_pre_pop(&store, &pre_pop, &pool);
            let mut total = Duration::ZERO;
            for i in 0..iters {
                reset_mdbx_pre_pop(&store, &pre_pop, &pool);
                let mut root =
                    SaltStateRoot::new(&store).with_min_par_batch_size(4).with_deferred_levels(2);
                let (elapsed, stats) = run_mdbx_blocks(&store, &mut root, &pool, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats(&format!("MDBX({SALT_NUM_THREADS}t)"), &stats);
                }
            }
            total
        })
    });

    // ---- FlatFile ----
    group.bench_function(BenchmarkId::new("flat", &label), |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::TempDir::new().unwrap();
            let store = FlatFileStore::new(dir.path()).unwrap();
            reset_flat_pre_pop(&store, &pre_pop, &pool);
            let mut total = Duration::ZERO;
            for i in 0..iters {
                reset_flat_pre_pop(&store, &pre_pop, &pool);
                let mut root =
                    SaltStateRoot::new(&store).with_min_par_batch_size(4).with_deferred_levels(2);
                let (elapsed, stats) = run_flat_blocks(&store, &mut root, &pool, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats(&format!("FlatFile({SALT_NUM_THREADS}t)"), &stats);
                }
            }
            total
        })
    });

    // ---- RocksDB (baseline) ----
    group.bench_function(BenchmarkId::new("rocksdb", &label), |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::TempDir::new().unwrap();
            let store = RocksSaltStore::new(dir.path()).unwrap();
            reset_rocks_pre_pop(&store, &pre_pop, &pool);
            let mut total = Duration::ZERO;
            for i in 0..iters {
                reset_rocks_pre_pop(&store, &pre_pop, &pool);
                let mut root =
                    SaltStateRoot::new(&store).with_min_par_batch_size(4).with_deferred_levels(2);
                let (elapsed, stats) = run_rocks_blocks(&store, &mut root, &pool, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats(&format!("RocksDB({SALT_NUM_THREADS}t)"), &stats);
                }
            }
            total
        })
    });

    // ---- AsyncRocksDB (in-memory reads + async persistence) ----
    group.bench_function(BenchmarkId::new("async_rocks", &label), |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::TempDir::new().unwrap();
            let store = AsyncRocksStore::new(dir.path()).unwrap();
            reset_async_rocks_pre_pop(&store, &pre_pop, &pool);
            let snap = store.snapshot();
            let mut total = Duration::ZERO;
            for i in 0..iters {
                store.restore(&snap);
                let mut root =
                    SaltStateRoot::new(&store).with_min_par_batch_size(4).with_deferred_levels(2);
                let (elapsed, stats) =
                    run_async_rocks_blocks(&store, &mut root, &pool, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats(&format!("AsyncRocks({SALT_NUM_THREADS}t)"), &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// QMDB benchmarks (separate groups to avoid thread/state interference)
// Run: cargo bench --bench store_compare -- "QMDB sync"
// Run: cargo bench --bench store_compare -- "QMDB pipeline"
// ---------------------------------------------------------------------------

use parking_lot::RwLock;
use qmdb::{
    config::Config as QmdbConfig,
    def::{IN_BLOCK_IDX_BITS, OP_CREATE, OP_WRITE},
    seqads::task::{SingleCsTask, TaskBuilder},
    tasks::TasksManager,
    AdsCore, AdsWrap, ADS,
};
use reth_primitives_traits::Account;
use xlayer_salt::account::{
    account_plain_key, encode_account, encode_storage_value, storage_plain_key,
};

fn bundle_state_to_qmdb_task(
    bundle: &revm_database::BundleState,
    op_type: u8,
) -> (SingleCsTask, usize) {
    let mut builder = TaskBuilder::new();
    let mut count = 0usize;

    for (address, bundle_account) in bundle.state() {
        let address = Address::from(*address);

        if let Some(info) = &bundle_account.info {
            let code_hash =
                if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
            let account =
                Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
            let key = account_plain_key(&address);
            let value = encode_account(&account);
            builder.add_op(op_type, &key, &value);
            count += 1;
        }

        for (slot, slot_info) in &bundle_account.storage {
            let slot_b256 = B256::from(*slot);
            let key = storage_plain_key(&address, &slot_b256);
            let value = encode_storage_value(&slot_info.present_value);
            builder.add_op(op_type, &key, &value);
            count += 1;
        }
    }

    (builder.build(), count)
}

const QMDB_PRE_POP_CHUNK_SIZE: usize = 20_000;

fn qmdb_pre_populate(ads: &mut AdsWrap<SingleCsTask>, pre_pop: &revm_database::BundleState) -> i64 {
    let accounts: Vec<_> = pre_pop.state().iter().collect();
    let num_chunks = (accounts.len() + QMDB_PRE_POP_CHUNK_SIZE - 1) / QMDB_PRE_POP_CHUNK_SIZE;

    for (chunk_idx, chunk) in accounts.chunks(QMDB_PRE_POP_CHUNK_SIZE).enumerate() {
        let height = (chunk_idx + 1) as i64;
        let mut builder = TaskBuilder::new();

        for (address, bundle_account) in chunk {
            let address = Address::from(**address);
            if let Some(info) = &bundle_account.info {
                let code_hash =
                    if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
                let account =
                    Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
                let key = account_plain_key(&address);
                let value = encode_account(&account);
                builder.add_op(OP_CREATE, &key, &value);
            }
            for (slot, slot_info) in &bundle_account.storage {
                let slot_b256 = B256::from(*slot);
                let key = storage_plain_key(&address, &slot_b256);
                let value = encode_storage_value(&slot_info.present_value);
                builder.add_op(OP_CREATE, &key, &value);
            }
        }

        let task = builder.build();
        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
    }

    ads.flush();
    num_chunks as i64
}

fn run_qmdb_blocks(
    ads: &mut AdsWrap<SingleCsTask>,
    block_bundles: &[revm_database::BundleState],
    start_height: i64,
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();

    for (i, bundle) in block_bundles.iter().enumerate() {
        let height = start_height + i as i64;
        let block_start = Instant::now();

        let prep_start = Instant::now();
        let (task, state_entries) = bundle_state_to_qmdb_task(bundle, OP_WRITE);
        let prep_time = prep_start.elapsed();

        let delta_start = Instant::now();
        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
        let delta_time = delta_start.elapsed();

        let root_start = Instant::now();
        ads.flush();
        let root_time = root_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: root_time,
            disk_io_time: Duration::ZERO,
            state_entries,
            trie_entries: 0,
        });
    }

    (total_start.elapsed(), block_stats)
}

fn run_qmdb_blocks_pipelined(
    ads: &mut AdsWrap<SingleCsTask>,
    block_bundles: &[revm_database::BundleState],
    start_height: i64,
    verbose: bool,
) -> Duration {
    let mut total_prep = Duration::ZERO;
    let mut total_submit = Duration::ZERO;

    let total_start = Instant::now();

    for (i, bundle) in block_bundles.iter().enumerate() {
        let height = start_height + i as i64;

        let prep_start = Instant::now();
        let (task, _) = bundle_state_to_qmdb_task(bundle, OP_WRITE);
        total_prep += prep_start.elapsed();

        let submit_start = Instant::now();
        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
        total_submit += submit_start.elapsed();
    }

    let flush_start = Instant::now();
    ads.flush();
    let flush_time = flush_start.elapsed();

    let total = total_start.elapsed();

    if verbose {
        let num = block_bundles.len() as u32;
        eprintln!(
            "─── QMDB(pipeline) ───\n  \
             {} blocks avg: {:.2?}  (prep {:.2?}  submit {:.2?}  flush {:.2?})\n",
            num,
            total / num,
            total_prep / num,
            total_submit / num,
            flush_time / num,
        );
    }

    total
}

/// Shared QMDB setup: generate data, create instance, pre-populate.
fn setup_qmdb(
) -> (AdsWrap<SingleCsTask>, Vec<revm_database::BundleState>, i64, String, tempfile::TempDir) {
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");
    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);

    let mut rng = StdRng::seed_from_u64(42);
    let pre_pop = generate_bundle_state_random(PRE_POP_ACCOUNTS, SLOTS_PER_ACCOUNT, &mut rng);
    let pre_pop_addresses: Vec<Address> = pre_pop.state().keys().copied().collect();

    let mut rng_blocks = StdRng::seed_from_u64(43);
    let block_bundles: Vec<_> = (0..NUM_BLOCKS)
        .map(|i| {
            generate_block_updates_from_addresses(
                &pre_pop_addresses,
                SLOTS_PER_ACCOUNT,
                i,
                &mut rng_blocks,
            )
        })
        .collect();

    eprintln!(
        "Setup: {} accounts, {} state changes/block",
        PRE_POP_ACCOUNTS, state_changes_per_block
    );

    let dir = tempfile::TempDir::new().unwrap();
    let config =
        QmdbConfig { dir: dir.path().to_str().unwrap().to_string(), ..QmdbConfig::default() };
    AdsCore::init_dir(&config);
    let mut ads = AdsWrap::<SingleCsTask>::new(&config);
    let num_pre_pop_blocks = qmdb_pre_populate(&mut ads, &pre_pop);
    let next_height = num_pre_pop_blocks + 1;

    (ads, block_bundles, next_height, label, dir)
}

fn bench_qmdb_sync(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB sync");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");
    let mut state: Option<(
        AdsWrap<SingleCsTask>,
        Vec<revm_database::BundleState>,
        i64,
        tempfile::TempDir,
    )> = None;

    group.bench_function(BenchmarkId::new("qmdb", &label), |b| {
        let (ads, block_bundles, next_height, _dir) = state.get_or_insert_with(|| {
            let (ads, bundles, h, _, dir) = setup_qmdb();
            (ads, bundles, h, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_qmdb_blocks(ads, block_bundles, *next_height);
                *next_height += NUM_BLOCKS as i64;
                total += elapsed;
                if i == 0 {
                    print_stats("QMDB", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

fn bench_qmdb_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB pipeline");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");
    let mut state: Option<(
        AdsWrap<SingleCsTask>,
        Vec<revm_database::BundleState>,
        i64,
        tempfile::TempDir,
    )> = None;

    group.bench_function(BenchmarkId::new("qmdb_pipeline", &label), |b| {
        let (ads, block_bundles, next_height, _dir) = state.get_or_insert_with(|| {
            let (ads, bundles, h, _, dir) = setup_qmdb();
            (ads, bundles, h, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let elapsed = run_qmdb_blocks_pipelined(ads, block_bundles, *next_height, i == 0);
                *next_height += NUM_BLOCKS as i64;
                total += elapsed;
            }
            total
        })
    });

    group.finish();
}

criterion_group!(benches, bench_store_compare, bench_qmdb_pipeline, bench_qmdb_sync);
criterion_main!(benches);
