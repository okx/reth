//! Benchmark: sei-db state commitment (MemIAVL) with EVM workloads.
//!
//! Compares sei-db CosmosOnly (MemIAVL only) vs DualWrite (MemIAVL + FlatKV) modes
//! vs reth native MPT+MDBX.
//! Uses the same workload generation as store_compare.rs for fair comparison.
//!
//! Run:    cargo bench --bench seidb_compare -p xlayer-salt
//! Cosmos: cargo bench --bench seidb_compare -p xlayer-salt -- "cosmos"
//! Dual:   cargo bench --bench seidb_compare -p xlayer-salt -- "dual"
//! MPT:    cargo bench --bench seidb_compare -p xlayer-salt -- "mpt"

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use revm_database::{states::StorageSlot, AccountStatus, BundleState, StorageWithOriginalValues};
use revm_state::AccountInfo;
use seidb::db::SeiDb;
use seidb_common::config::{MemIavlConfig, StateCommitConfig, StateStoreConfig, WriteMode};
use seidb_traits::ss::StateStore;
use std::time::{Duration, Instant};
use xlayer_salt::seidb_adapter;

use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    ProviderFactory, StateWriter, TrieWriter,
};
use reth_trie::{updates::TrieUpdates, HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;

const PRE_POP_ACCOUNTS: usize = 200_000;
const NUM_BLOCKS: usize = 10;
const ACCOUNTS_PER_BLOCK: usize = 2000;
const SLOTS_PER_ACCOUNT: usize = 10;
/// Chunk size for pre-population commits (avoid huge single changeset).
const PRE_POP_CHUNK_SIZE: usize = 20_000;

// ---------------------------------------------------------------------------
// Data generation (same as store_compare.rs)
// ---------------------------------------------------------------------------

fn generate_bundle_state_random(
    num_accounts: usize,
    slots_per_account: usize,
    rng: &mut StdRng,
) -> BundleState {
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

    BundleState {
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
) -> BundleState {
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

    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

fn bundle_state_changes(bundle: &BundleState) -> usize {
    bundle.state().iter().map(|(_, a)| 1 + a.storage.len()).sum()
}

// ---------------------------------------------------------------------------
// Per-block stats
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BlockStats {
    wall_time: Duration,
    apply_time: Duration,
    commit_time: Duration,
    state_changes: usize,
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
    eprintln!("--- {} ---", label);
    eprintln!(
        "  {} blocks avg: {:.2?}  (apply {:.2?}  commit {:.2?})",
        n,
        avg(|s| s.wall_time),
        avg(|s| s.apply_time),
        avg(|s| s.commit_time),
    );
    eprintln!("  changes/blk: {:.0}", avg_usize(|s| s.state_changes));
    eprintln!();
}

// ---------------------------------------------------------------------------
// sei-db helpers
// ---------------------------------------------------------------------------

/// Cosmos module names matching sei-chain's real module set.
const COSMOS_MODULES: &[&str] =
    &["acc", "bank", "distribution", "evm", "feegrant", "gov", "mint", "slashing", "staking"];

/// Open a SeiDb with the given WriteMode, initialize stores, and pre-populate.
fn setup_seidb(dir: &std::path::Path, write_mode: WriteMode, pre_pop: &BundleState) -> SeiDb {
    setup_seidb_inner(dir, write_mode, pre_pop, false)
}

/// Open a SeiDb with multi-module + async apply enabled.
fn setup_seidb_parallel(
    dir: &std::path::Path,
    write_mode: WriteMode,
    pre_pop: &BundleState,
) -> SeiDb {
    setup_seidb_inner(dir, write_mode, pre_pop, true)
}

fn setup_seidb_inner(
    dir: &std::path::Path,
    write_mode: WriteMode,
    pre_pop: &BundleState,
    async_apply: bool,
) -> SeiDb {
    let home = dir.to_string_lossy().to_string();
    let sc_config = StateCommitConfig {
        write_mode,
        memiavl: MemIavlConfig {
            snapshot_interval: 0, // disable auto-snapshot for benchmarks
            async_commit_buffer: if async_apply { 100 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };

    let mut db = SeiDb::open(&home, sc_config, None).unwrap();
    let stores: Vec<String> = COSMOS_MODULES.iter().map(|s| s.to_string()).collect();
    db.initialize(&stores);
    db.load_version(0).unwrap();

    // Pre-populate in chunks (use multimodule distribution when async)
    let chunks = if async_apply {
        seidb_adapter::bundle_to_multimodule_pre_populate_changesets(pre_pop, PRE_POP_CHUNK_SIZE)
    } else {
        seidb_adapter::bundle_to_pre_populate_changesets(pre_pop, PRE_POP_CHUNK_SIZE)
    };
    for chunk in &chunks {
        db.sc_mut().apply_change_sets(chunk).unwrap();
        db.sc_mut().commit().unwrap();
    }

    db
}

/// Run benchmark blocks through sei-db, returning total duration and per-block stats.
fn run_seidb_blocks(db: &mut SeiDb, block_bundles: &[BundleState]) -> (Duration, Vec<BlockStats>) {
    run_seidb_blocks_inner(db, block_bundles, false)
}

/// Run benchmark blocks using multi-module changeset distribution.
fn run_seidb_blocks_multimodule(
    db: &mut SeiDb,
    block_bundles: &[BundleState],
) -> (Duration, Vec<BlockStats>) {
    run_seidb_blocks_inner(db, block_bundles, true)
}

fn run_seidb_blocks_inner(
    db: &mut SeiDb,
    block_bundles: &[BundleState],
    multimodule: bool,
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();

    for bundle in block_bundles {
        let block_start = Instant::now();

        // Convert BundleState to sei-db changesets
        let changesets = if multimodule {
            seidb_adapter::bundle_to_multimodule_changesets(bundle)
        } else {
            seidb_adapter::bundle_to_changesets(bundle)
        };
        let state_changes = bundle_state_changes(bundle);

        // Apply changesets
        let apply_start = Instant::now();
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        let apply_time = apply_start.elapsed();

        // Commit (includes IAVL hash computation)
        let commit_start = Instant::now();
        db.sc_mut().commit().unwrap();
        let commit_time = commit_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            apply_time,
            commit_time,
            state_changes,
        });
    }

    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_seidb_cosmos_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("SeiDB cosmos-only");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");

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
        "SeiDB cosmos-only setup: {} accounts, {} state changes/block",
        PRE_POP_ACCOUNTS, state_changes_per_block
    );

    let mut state: Option<(SeiDb, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("seidb_cosmos_only", &label), |b| {
        let (db, _dir) = state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb(dir.path(), WriteMode::CosmosOnly, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("SeiDB CosmosOnly (MemIAVL)", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

fn bench_seidb_dual_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("SeiDB dual-write");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");

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
        "SeiDB dual-write setup: {} accounts, {} state changes/block",
        PRE_POP_ACCOUNTS, state_changes_per_block
    );

    let mut state: Option<(SeiDb, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("seidb_dual_write", &label), |b| {
        let (db, _dir) = state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb(dir.path(), WriteMode::DualWrite, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("SeiDB DualWrite (MemIAVL + FlatKV)", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// MPT+MDBX helpers
// ---------------------------------------------------------------------------

/// Pre-populate MPT+MDBX with the given bundle state.
fn mpt_pre_populate(factory: &ProviderFactory<MockNodeTypesWithDB>, pre_pop: &BundleState) {
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(pre_pop.state());
    let sorted = hashed.into_sorted();
    let rw = factory.provider_rw().unwrap();
    rw.write_hashed_state(&sorted).unwrap();
    let (_, updates): (B256, TrieUpdates) =
        StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
    rw.write_trie_updates(updates).unwrap();
    rw.commit().unwrap();
}

/// Run benchmark blocks through MPT+MDBX, returning total duration and per-block stats.
fn run_mpt_blocks(
    factory: &ProviderFactory<MockNodeTypesWithDB>,
    block_bundles: &[BundleState],
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();

    for bundle in block_bundles {
        let block_start = Instant::now();
        let state_changes = bundle_state_changes(bundle);

        // Step 1: Convert BundleState → HashedPostState (keccak256 hashing)
        let apply_start = Instant::now();
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let sorted_state = hashed.into_sorted();
        let apply_time = apply_start.elapsed();

        // Step 2: Compute MPT state root
        let rw = factory.provider_rw().unwrap();
        let commit_start = Instant::now();
        let (_, updates) =
            StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted_state).unwrap();

        // Step 3: Write state + trie updates to MDBX
        rw.write_hashed_state(&sorted_state).unwrap();
        rw.write_trie_updates(updates).unwrap();
        rw.commit().unwrap();
        let commit_time = commit_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            apply_time,
            commit_time,
            state_changes,
        });
    }

    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// MPT+MDBX benchmark
// ---------------------------------------------------------------------------

fn bench_mpt_mdbx(c: &mut Criterion) {
    let mut group = c.benchmark_group("MPT mdbx");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");

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
        "MPT+MDBX setup: {} accounts, {} state changes/block",
        PRE_POP_ACCOUNTS, state_changes_per_block
    );

    group.bench_function(BenchmarkId::new("mpt_mdbx", &label), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                // Each iteration uses a fresh factory to start from the same state
                let factory = create_test_provider_factory();
                eprintln!("  Pre-populating MPT with {} accounts...", PRE_POP_ACCOUNTS);
                mpt_pre_populate(&factory, &pre_pop);
                eprintln!("  MPT pre-pop done.");

                let (elapsed, stats) = run_mpt_blocks(&factory, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("reth MPT+MDBX", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Go-equivalent workload: 100 keys/block (matching sei-db Go bench)
// ---------------------------------------------------------------------------

/// Go sei-db benchmark uses: 100K accounts pre-pop, 100 keys/block, 1000 blocks.
/// This matches that workload for fair TPS comparison.
const GO_PRE_POP: usize = 100_000;
const GO_KEYS_PER_BLOCK: usize = 100;
const GO_NUM_BLOCKS: usize = 100; // use 100 blocks per criterion iteration
const GO_SLOTS_PER_ACCOUNT: usize = 0; // Go bench uses flat key-value, no storage slots

fn generate_go_style_bundle(
    addresses: &[Address],
    block_index: usize,
    rng: &mut StdRng,
) -> BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for i in 0..GO_KEYS_PER_BLOCK {
        let idx = rng.random_range(0..addresses.len());
        let addr = addresses[idx];
        let nonce = (block_index * GO_KEYS_PER_BLOCK + i) as u64;
        let balance = U256::from(nonce + 1);
        let info =
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, account_id: None, code: None };
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info),
                original_info: None,
                status: AccountStatus::Changed,
                storage: StorageWithOriginalValues::default(),
            },
        );
    }

    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

fn bench_seidb_go_equivalent(c: &mut Criterion) {
    let mut group = c.benchmark_group("SeiDB go-equivalent");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let label = format!("pre{GO_PRE_POP}_{GO_NUM_BLOCKS}blk_{GO_KEYS_PER_BLOCK}keys");

    // Pre-populate with 100K accounts (account-only, no storage slots)
    let mut rng = StdRng::seed_from_u64(42);
    let pre_pop = generate_bundle_state_random(GO_PRE_POP, GO_SLOTS_PER_ACCOUNT, &mut rng);
    let pre_pop_addresses: Vec<Address> = pre_pop.state().keys().copied().collect();

    // Generate 100 blocks × 100 keys each
    let mut rng_blocks = StdRng::seed_from_u64(43);
    let block_bundles: Vec<_> = (0..GO_NUM_BLOCKS)
        .map(|i| generate_go_style_bundle(&pre_pop_addresses, i, &mut rng_blocks))
        .collect();

    eprintln!(
        "SeiDB go-equivalent setup: {} pre-pop accounts, {} keys/block, {} blocks",
        GO_PRE_POP, GO_KEYS_PER_BLOCK, GO_NUM_BLOCKS
    );

    // --- CosmosOnly ---
    let mut cosmos_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("cosmos_only", &label), |b| {
        let (db, _dir) = cosmos_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb(dir.path(), WriteMode::CosmosOnly, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    let total_keys: usize = stats.iter().map(|s| s.state_changes).sum();
                    let tps = total_keys as f64 / elapsed.as_secs_f64();
                    eprintln!("--- Go-equiv CosmosOnly ---");
                    eprintln!(
                        "  {} blocks, {} total keys, {:.2?} total, {:.0} TPS",
                        GO_NUM_BLOCKS, total_keys, elapsed, tps,
                    );
                    print_stats("Go-equiv CosmosOnly detail", &stats);
                }
            }
            total
        })
    });

    // --- DualWrite ---
    let mut dual_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("dual_write", &label), |b| {
        let (db, _dir) = dual_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb(dir.path(), WriteMode::DualWrite, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    let total_keys: usize = stats.iter().map(|s| s.state_changes).sum();
                    let tps = total_keys as f64 / elapsed.as_secs_f64();
                    eprintln!("--- Go-equiv DualWrite ---");
                    eprintln!(
                        "  {} blocks, {} total keys, {:.2?} total, {:.0} TPS",
                        GO_NUM_BLOCKS, total_keys, elapsed, tps,
                    );
                    print_stats("Go-equiv DualWrite detail", &stats);
                }
            }
            total
        })
    });

    // --- MPT+MDBX for comparison ---
    group.bench_function(BenchmarkId::new("mpt_mdbx", &label), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let factory = create_test_provider_factory();
                mpt_pre_populate(&factory, &pre_pop);
                let (elapsed, stats) = run_mpt_blocks(&factory, &block_bundles);
                total += elapsed;
                if i == 0 {
                    let total_keys: usize = stats.iter().map(|s| s.state_changes).sum();
                    let tps = total_keys as f64 / elapsed.as_secs_f64();
                    eprintln!("--- Go-equiv MPT+MDBX ---");
                    eprintln!(
                        "  {} blocks, {} total keys, {:.2?} total, {:.0} TPS",
                        GO_NUM_BLOCKS, total_keys, elapsed, tps,
                    );
                    print_stats("Go-equiv MPT+MDBX detail", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Parallel multi-module benchmark (matches Go async_commit)
// ---------------------------------------------------------------------------

fn bench_seidb_parallel(c: &mut Criterion) {
    let mut group = c.benchmark_group("SeiDB parallel");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");

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
        "SeiDB parallel setup: {} accounts, {} state changes/block, {} modules",
        PRE_POP_ACCOUNTS,
        state_changes_per_block,
        COSMOS_MODULES.len(),
    );

    // --- CosmosOnly parallel ---
    let mut cosmos_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("cosmos_only_parallel", &label), |b| {
        let (db, _dir) = cosmos_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb_parallel(dir.path(), WriteMode::CosmosOnly, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks_multimodule(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("SeiDB CosmosOnly Parallel (9 modules)", &stats);
                }
            }
            total
        })
    });

    // --- DualWrite parallel ---
    let mut dual_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("dual_write_parallel", &label), |b| {
        let (db, _dir) = dual_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb_parallel(dir.path(), WriteMode::DualWrite, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks_multimodule(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("SeiDB DualWrite Parallel (9 modules)", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// SC + SS full-stack benchmark (includes RocksDB MVCC write)
// ---------------------------------------------------------------------------

/// Open SeiDb with both SC and SS layers enabled.
fn setup_seidb_full_stack(
    dir: &std::path::Path,
    write_mode: WriteMode,
    pre_pop: &BundleState,
    async_apply: bool,
) -> SeiDb {
    let home = dir.to_string_lossy().to_string();
    let sc_config = StateCommitConfig {
        write_mode,
        memiavl: MemIavlConfig {
            snapshot_interval: 0,
            async_commit_buffer: if async_apply { 100 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };
    let ss_config = StateStoreConfig {
        enable: true,
        db_directory: dir.join("cosmos_ss").to_string_lossy().to_string(),
        evm_db_directory: dir.join("evm_ss").to_string_lossy().to_string(),
        keep_last_version: true,
        ..Default::default()
    };

    let mut db = SeiDb::open(&home, sc_config, Some(ss_config)).unwrap();
    let stores: Vec<String> = COSMOS_MODULES.iter().map(|s| s.to_string()).collect();
    db.initialize(&stores);
    db.load_version(0).unwrap();

    // Pre-populate SC (SS pre-population skipped for brevity — benchmark
    // measures steady-state write throughput, not first-block cold start)
    let chunks = if async_apply {
        seidb_adapter::bundle_to_multimodule_pre_populate_changesets(pre_pop, PRE_POP_CHUNK_SIZE)
    } else {
        seidb_adapter::bundle_to_pre_populate_changesets(pre_pop, PRE_POP_CHUNK_SIZE)
    };
    for (i, chunk) in chunks.iter().enumerate() {
        db.sc_mut().apply_change_sets(chunk).unwrap();
        let ver = db.sc_mut().commit().unwrap();
        // Also write to SS
        if let Some(ss) = db.ss() {
            ss.apply_changeset_sync(ver, chunk).unwrap();
        }
        if i == 0 {
            eprintln!("  Pre-populating SC+SS...");
        }
    }
    eprintln!("  Pre-pop done ({} chunks)", chunks.len());

    db
}

/// Run blocks through SC + SS, measuring total time including RocksDB writes.
fn run_seidb_full_stack(
    db: &mut SeiDb,
    block_bundles: &[BundleState],
    multimodule: bool,
) -> (Duration, Vec<BlockStats>) {
    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();

    for bundle in block_bundles {
        let block_start = Instant::now();

        let changesets = if multimodule {
            seidb_adapter::bundle_to_multimodule_changesets(bundle)
        } else {
            seidb_adapter::bundle_to_changesets(bundle)
        };
        let state_changes = bundle_state_changes(bundle);

        // SC apply
        let apply_start = Instant::now();
        db.sc_mut().apply_change_sets(&changesets).unwrap();
        let apply_time = apply_start.elapsed();

        // SC commit (hash computation)
        let commit_start = Instant::now();
        let ver = db.sc_mut().commit().unwrap();

        // SS write (RocksDB MVCC — the real disk I/O)
        if let Some(ss) = db.ss() {
            ss.apply_changeset_sync(ver, &changesets).unwrap();
        }
        let commit_time = commit_start.elapsed();

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            apply_time,
            commit_time, // includes both SC commit + SS write
            state_changes,
        });
    }

    (total_start.elapsed(), block_stats)
}

fn bench_seidb_full_stack(c: &mut Criterion) {
    let mut group = c.benchmark_group("SeiDB full-stack SC+SS");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");

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
        "SeiDB full-stack setup: {} accounts, {} state changes/block (SC+SS)",
        PRE_POP_ACCOUNTS, state_changes_per_block,
    );

    // --- SC-only (no SS, for comparison baseline within same group) ---
    let mut sc_only_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("sc_only", &label), |b| {
        let (db, _dir) = sc_only_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb(dir.path(), WriteMode::CosmosOnly, &pre_pop);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_blocks(db, &block_bundles);
                total += elapsed;
                if i == 0 {
                    print_stats("SC-only (no SS)", &stats);
                }
            }
            total
        })
    });

    // --- SC + SS full stack ---
    let mut full_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("sc_plus_ss", &label), |b| {
        let (db, _dir) = full_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb_full_stack(dir.path(), WriteMode::CosmosOnly, &pre_pop, false);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_full_stack(db, &block_bundles, false);
                total += elapsed;
                if i == 0 {
                    print_stats("SC + SS (RocksDB MVCC)", &stats);
                }
            }
            total
        })
    });

    // --- SC + SS full stack parallel (9 modules) ---
    let mut full_par_state: Option<(SeiDb, tempfile::TempDir)> = None;
    group.bench_function(BenchmarkId::new("sc_plus_ss_parallel", &label), |b| {
        let (db, _dir) = full_par_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let db = setup_seidb_full_stack(dir.path(), WriteMode::CosmosOnly, &pre_pop, true);
            (db, dir)
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let (elapsed, stats) = run_seidb_full_stack(db, &block_bundles, true);
                total += elapsed;
                if i == 0 {
                    print_stats("SC + SS Parallel (9 modules)", &stats);
                }
            }
            total
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion main
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_seidb_cosmos_only,
    bench_seidb_dual_write,
    bench_mpt_mdbx,
    bench_seidb_go_equivalent,
    bench_seidb_parallel,
    bench_seidb_full_stack,
);
criterion_main!(benches);
