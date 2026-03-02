//! Single end-to-end benchmark: MPT vs SALT.
//!
//! Two scenarios: (1) **ERC20**: deploy one ERC20, airdrop to 200k accounts, then 10 blocks
//! of 2000 ERC20 transfers each. (2) **Random**: 200k random accounts, 10 blocks × 2000 updates.
//!
//! **MPT storage_trie_writes=0**: For shallow per-account storage tries (e.g. ~10 slots),
//! reth's `StorageTrieUpdates::finalize` filters root-level nodes (`exclude_empty_from_pair` in
//! `reth_trie_common::updates`), and `insert_storage_updates` skips when
//! `storage_updates.is_empty()`. So we see 0 storage tries/nodes; storage roots are still in
//! account RLP via `write_hashed_state`. **disk_write_ops**: Storage root updates are written in
//! account records (state), not as separate trie nodes, so the real MPT work (keccak per account)
//! is in state writes; trie count undercounts.

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, map::HashMap as PrimitivesHashMap, Address, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::{
    io::Write,
    time::{Duration, Instant},
};

use rayon::ThreadPoolBuilder;
use salt::{EphemeralSaltState, StateRoot as SaltStateRoot};
use xlayer_salt::{convert::bundle_state_to_plain_kv, rocks_store::RocksSaltStore};

use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    ProviderFactory, StateWriter, TrieWriter,
};
use reth_trie::{
    updates::TrieUpdates, HashedPostState, HashedPostStateSorted, KeccakKeyHasher, StateRoot,
};
use reth_trie_db::DatabaseStateRoot;

/// Pre-population size. Use 400_000 if 200_000 is not enough.
const PRE_POP_ACCOUNTS: usize = 200_000;
/// Blocks to process after pre-pop.
const NUM_BLOCKS: usize = 10;
/// Accounts updated per block.
const ACCOUNTS_PER_BLOCK: usize = 2000;
/// Storage slots per account.
const SLOTS_PER_ACCOUNT: usize = 10;
/// SALT IPA thread count.
const SALT_NUM_THREADS: usize = 8;

/// Fixed ERC20 contract address for the benchmark (deployed once).
fn erc20_contract() -> Address {
    Address::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
}

// ---------------------------------------------------------------------------
// ERC20-style state (one contract, balance mapping = slot 0)
// ---------------------------------------------------------------------------

/// ERC20 `balanceOf` mapping slot key: `keccak256(abi.encode(address, 0))`.
fn erc20_balance_slot(holder: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    // buf[32..64] stays 0 (slot 0)
    B256::from(keccak256(buf))
}

/// Pre-pop: deploy one ERC20 (one account) and airdrop to 200k addresses (200k storage slots).
fn generate_erc20_pre_pop(
    num_holders: usize,
    rng: &mut StdRng,
) -> (revm_database::BundleState, Vec<Address>) {
    let mut addr_buf = [0u8; 20];
    let mut holders = Vec::with_capacity(num_holders);
    for _ in 0..num_holders {
        rng.fill_bytes(&mut addr_buf);
        holders.push(Address::from(addr_buf));
    }

    let initial_balance = U256::from(1_000_000u64);
    let mut storage = StorageWithOriginalValues::default();
    for &addr in &holders {
        let slot = erc20_balance_slot(addr);
        storage.insert(slot.into(), StorageSlot::new_changed(U256::ZERO, initial_balance));
    }

    let contract_account = revm_database::BundleAccount {
        info: Some(AccountInfo {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        }),
        original_info: None,
        status: AccountStatus::Changed,
        storage,
    };

    let mut state = PrimitivesHashMap::default();
    state.insert(erc20_contract(), contract_account);

    let bundle = revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    };
    (bundle, holders)
}

/// One block: 2000 ERC20 transfers (sender -> receiver). Updates only contract storage: 4000 slots.
fn generate_erc20_block_transfers(
    holders: &[Address],
    _block_index: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut storage = StorageWithOriginalValues::default();

    for _ in 0..ACCOUNTS_PER_BLOCK {
        let si = rng.random_range(0..holders.len());
        let ri = rng.random_range(0..holders.len());
        let (sender, receiver) = if si == ri {
            let ri2 = (ri + 1) % holders.len();
            (holders[si], holders[ri2])
        } else {
            (holders[si], holders[ri])
        };
        let sender_slot = erc20_balance_slot(sender);
        let receiver_slot = erc20_balance_slot(receiver);
        storage.insert(
            sender_slot.into(),
            StorageSlot::new_changed(U256::from(1_000_000u64), U256::from(999_900u64)),
        );
        storage.insert(
            receiver_slot.into(),
            StorageSlot::new_changed(U256::from(1_000_000u64), U256::from(1_000_100u64)),
        );
    }

    let contract_account = revm_database::BundleAccount {
        info: Some(AccountInfo {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        }),
        original_info: None,
        status: AccountStatus::Changed,
        storage,
    };

    let mut state = PrimitivesHashMap::default();
    state.insert(erc20_contract(), contract_account);

    revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

// ---------------------------------------------------------------------------
// Test data generation (random accounts)
// ---------------------------------------------------------------------------

/// Generates a bundle with **random** 20-byte addresses (for pre-population).
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

/// Generates one block of updates: 2000 accounts **randomly chosen from** `addresses`,
/// with new balance/nonce/storage (simulates 更新/转账 on existing accounts).
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

/// Count state changes in a bundle: for each account, 1 + storage.len().
fn bundle_state_changes(bundle: &revm_database::BundleState) -> usize {
    bundle.state().iter().map(|(_, a)| 1 + a.storage.len()).sum()
}

// ---------------------------------------------------------------------------
// Per-block stats
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BlockStats {
    wall_time: Duration,
    disk_write_ops: usize,
    /// Actual state/bucket entries written (SALT: ws.entries; MPT: 0, not tracked per block).
    state_write_ops: usize,
    trie_node_disk_writes: usize,
    /// MPT only: account trie nodes written (path from leaf to root per account).
    account_trie_writes: usize,
    /// MPT only: storage trie nodes written (per-account storage trie).
    storage_trie_writes: usize,
    /// Pure CPU: BundleState → internal format.
    state_prep_time: Duration,
    /// SALT only: read existing state from store + compute bucket delta.
    state_delta_time: Duration,
    /// Root hash computation (MPT: read trie + Keccak; SALT: in-memory IPA).
    root_compute_time: Duration,
    /// Disk persistence (state + trie writes + commit/fsync).
    disk_io_time: Duration,
}

// ---------------------------------------------------------------------------
// MPT: prepare once (pre-pop), then run N blocks (no repeated preparation)
// ---------------------------------------------------------------------------

/// One-time setup: build hashed state + trie updates for pre-pop. Call once; then use
/// `run_mpt_blocks_only` with the returned (sorted, updates) for each benchmark iteration.
fn prepare_mpt_once(
    pre_pop: &revm_database::BundleState,
) -> (HashedPostStateSorted, TrieUpdates, Duration) {
    let start = Instant::now();
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(pre_pop.state());
    let sorted = hashed.into_sorted();
    let factory = create_test_provider_factory();
    let rw = factory.provider_rw().unwrap();
    rw.write_hashed_state(&sorted).unwrap();
    let (_, updates): (B256, TrieUpdates) =
        StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
    rw.write_trie_updates(updates.clone()).unwrap();
    rw.commit().unwrap();
    (sorted, updates, start.elapsed())
}

/// Run N blocks using the given persistent factory. Pre_pop state is (re-)applied each call so
/// each iteration starts from the same state; the same DB (and warm mmap) is reused.
///
/// When `debug_first_block` is true, prints one [MPT] diagnostic line for the first block only.
fn run_mpt_blocks_only(
    factory: &ProviderFactory<MockNodeTypesWithDB>,
    pre_pop_sorted: &HashedPostStateSorted,
    pre_pop_updates: &TrieUpdates,
    block_bundles: &[revm_database::BundleState],
    debug_first_block: bool,
) -> (Duration, Vec<BlockStats>) {
    if debug_first_block {
        eprintln!("  [MPT] Applying pre-pop to DB (this may take 10–60s)...");
        let _ = std::io::stderr().flush();
    }
    let rw = factory.provider_rw().unwrap();
    rw.write_hashed_state(pre_pop_sorted).unwrap();
    rw.write_trie_updates(pre_pop_updates.clone()).unwrap();
    rw.commit().unwrap();
    if debug_first_block {
        eprintln!("  [MPT] Pre-pop done, processing {} blocks...", block_bundles.len());
        let _ = std::io::stderr().flush();
    }

    let mut block_stats = Vec::with_capacity(block_bundles.len());
    let total_start = Instant::now();

    let mut first_block = debug_first_block;
    for bundle in block_bundles {
        let block_start = Instant::now();
        let state_changes = bundle_state_changes(bundle);

        let prep_start = Instant::now();
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let prefix_sets_mut = hashed.construct_prefix_sets();
        let n_storage_accounts = prefix_sets_mut.storage_prefix_sets.len();
        let total_storage_slots: usize =
            prefix_sets_mut.storage_prefix_sets.values().map(|ps| ps.len()).sum();
        let sorted_state = hashed.into_sorted();
        let prep_time = prep_start.elapsed();

        let rw = factory.provider_rw().unwrap();
        // Match reth production: compute root from overlay only (no write_hashed_state before).
        let root_start = Instant::now();
        let (_, updates) =
            StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted_state).unwrap();
        let root_time = root_start.elapsed();

        let io_start = Instant::now();
        rw.write_hashed_state(&sorted_state).unwrap();
        let state_write_time = io_start.elapsed();

        let account_trie_writes = updates.account_nodes.len();
        let storage_trie_writes: usize = updates.storage_tries.values().map(|s| s.len()).sum();

        if first_block {
            first_block = false;
            // reth: StorageTrieUpdates::finalize() filters root-level nodes
            // (exclude_empty_from_pair); insert_storage_updates() skips when
            // updates.is_empty(), so 0 storage_tries. Storage roots are still computed
            // and stored in account RLP (write_hashed_state).
            eprintln!(
                "  [MPT] prefix: {} storage accounts, {} storage slots  →  updates: {} account nodes, {} storage tries, {} storage nodes  (0 storage = shallow tries: root-level nodes filtered + empty not inserted by reth)",
                n_storage_accounts,
                total_storage_slots,
                updates.account_nodes.len(),
                updates.storage_tries.len(),
                storage_trie_writes,
            );
            let _ = std::io::stderr().flush();
        }

        let trie_start = Instant::now();
        let trie_entries = rw.write_trie_updates(updates).unwrap();
        let trie_only_time = trie_start.elapsed();
        let commit_start = Instant::now();
        rw.commit().unwrap();
        let commit_time = commit_start.elapsed();
        let trie_write_time = trie_only_time + commit_time;

        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            disk_write_ops: state_changes + trie_entries,
            state_write_ops: 0,
            trie_node_disk_writes: trie_entries,
            account_trie_writes,
            storage_trie_writes,
            state_prep_time: prep_time,
            state_delta_time: Duration::ZERO,
            root_compute_time: root_time,
            disk_io_time: state_write_time + trie_write_time,
        });
    }

    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// SALT: reset store to pre-pop state (state then trie). Rebuild root after this.
// ---------------------------------------------------------------------------

fn reset_salt_to_pre_pop(
    store: &RocksSaltStore,
    pre_pop: &revm_database::BundleState,
    pool: &rayon::ThreadPool,
) {
    let kvs = bundle_state_to_plain_kv(pre_pop);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_deferred_levels(3).with_min_par_batch_size(16);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state(state_updates).unwrap();
    store.update_trie(trie_updates).unwrap();
}

/// Run N blocks only (store must already be at pre-pop state; root fresh after reset).
fn run_salt_blocks_only(
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
        let ws = store.update_state(state_updates).unwrap();
        let trie_entries = store.update_trie(trie_updates).unwrap();
        let io_time = io_start.elapsed();
        block_stats.push(BlockStats {
            wall_time: block_start.elapsed(),
            disk_write_ops: ws.entries + trie_entries,
            state_write_ops: ws.entries,
            trie_node_disk_writes: trie_entries,
            account_trie_writes: 0,
            storage_trie_writes: 0,
            state_prep_time: prep_time,
            state_delta_time: delta_time,
            root_compute_time: root_time,
            disk_io_time: io_time,
        });
    }
    (total_start.elapsed(), block_stats)
}

// ---------------------------------------------------------------------------
// Print 10-block stats only (no pre-pop in log; setup is printed once by caller)
// ---------------------------------------------------------------------------

fn print_blocks_stats(
    label: &str,
    engine: &str,
    stats: &[BlockStats],
    state_changes_per_block: usize,
) {
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

    let avg_block = avg(|s| s.wall_time);
    let avg_prep = avg(|s| s.state_prep_time);
    let avg_delta = avg(|s| s.state_delta_time);
    let avg_root = avg(|s| s.root_compute_time);
    let avg_io = avg(|s| s.disk_io_time);
    let avg_disk_ops = avg_usize(|s| s.disk_write_ops);
    let avg_state_writes = avg_usize(|s| s.state_write_ops);
    let avg_trie_writes = avg_usize(|s| s.trie_node_disk_writes);
    let avg_account_trie = avg_usize(|s| s.account_trie_writes);
    let avg_storage_trie = avg_usize(|s| s.storage_trie_writes);
    let write_amp = avg_disk_ops / state_changes_per_block as f64;

    eprintln!("─── {engine} [{label}] ───");
    if avg_delta > Duration::ZERO {
        eprintln!("  {} blocks avg: {avg_block:.2?}  (prep {avg_prep:.2?}  delta {avg_delta:.2?}  root {avg_root:.2?}  io {avg_io:.2?})", n);
    } else {
        eprintln!("  {} blocks avg: {avg_block:.2?}  (prep {avg_prep:.2?}  root {avg_root:.2?}  io {avg_io:.2?})", n);
    }
    if avg_trie_writes > 0.0 ||
        avg_account_trie > 0.0 ||
        avg_storage_trie > 0.0 ||
        avg_state_writes > 0.0
    {
        let state_label = if avg_state_writes > 0.0 {
            format!("state: {avg_state_writes:.0}")
        } else {
            format!("state: {state_changes_per_block}")
        };
        eprintln!("  disk writes/blk: {avg_disk_ops:.0}  ({state_label}, trie: {avg_trie_writes:.0} = account {avg_account_trie:.0} + storage {avg_storage_trie:.0})  amp: {write_amp:.2}x");
        if avg_storage_trie == 0.0 && state_changes_per_block > 0 {
            eprintln!("  → storage_trie=0: reth filters root-level nodes and omits empty storage updates; storage root is in account RLP.");
        }
    } else {
        eprintln!("  disk writes/blk: {avg_disk_ops:.0}  (state: {state_changes_per_block}, trie: 0)  amp: {write_amp:.2}x");
    }
    eprintln!();
}

// ---------------------------------------------------------------------------
// Single benchmark: pre-pop 200k random accounts, 10 blocks × 2000 updates (from that set)
// ---------------------------------------------------------------------------

fn bench_mpt_vs_salt(c: &mut Criterion) {
    let mut group = c.benchmark_group("Block processing");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    // Shared thread pool for SALT (avoid spawn/teardown per criterion iteration)
    let pool = ThreadPoolBuilder::new().num_threads(SALT_NUM_THREADS).build().unwrap();

    // ---- ERC20 scenario: 1 contract, 200k holders, 10 blocks × 2000 transfers ----
    let label_erc20 =
        format!("erc20_1contract_200k_holders_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}xfer");
    let mut rng_erc20 = StdRng::seed_from_u64(40);
    let (erc20_pre_pop, erc20_holders) = generate_erc20_pre_pop(PRE_POP_ACCOUNTS, &mut rng_erc20);
    let erc20_pre_pop_entries = bundle_state_changes(&erc20_pre_pop);
    let mut rng_erc20_blocks = StdRng::seed_from_u64(41);
    let erc20_block_bundles: Vec<_> = (0..NUM_BLOCKS)
        .map(|i| generate_erc20_block_transfers(&erc20_holders, i, &mut rng_erc20_blocks))
        .collect();
    let erc20_state_changes_per_block = bundle_state_changes(&erc20_block_bundles[0]);

    // MPT: prepare once, then only 10 blocks per iteration
    let (erc20_mpt_sorted, erc20_mpt_updates, erc20_setup_elapsed) =
        prepare_mpt_once(&erc20_pre_pop);
    eprintln!(
        "Setup (once): 1 contract, {} entries  →  {:.2?}",
        erc20_pre_pop_entries, erc20_setup_elapsed
    );

    group.bench_function(BenchmarkId::new("mpt", &label_erc20), |b| {
        b.iter_custom(|iters| {
            let factory = create_test_provider_factory();
            let mut total = Duration::ZERO;
            let mut first_stats: Option<Vec<BlockStats>> = None;
            if iters > 0 {
                let _ = run_mpt_blocks_only(
                    &factory,
                    &erc20_mpt_sorted,
                    &erc20_mpt_updates,
                    &erc20_block_bundles,
                    false,
                );
            }
            for i in 0..iters {
                let (elapsed, stats) = run_mpt_blocks_only(
                    &factory,
                    &erc20_mpt_sorted,
                    &erc20_mpt_updates,
                    &erc20_block_bundles,
                    i == 0,
                );
                total += elapsed;
                if i == 0 {
                    first_stats = Some(stats);
                }
            }
            if let Some(stats) = first_stats {
                print_blocks_stats(&label_erc20, "MPT", &stats, erc20_state_changes_per_block);
            }
            total
        })
    });

    group.bench_function(
        BenchmarkId::new(&format!("salt_{SALT_NUM_THREADS}t"), &label_erc20),
        |b| {
            b.iter_custom(|iters| {
                let dir = tempfile::TempDir::new().unwrap();
                let store = RocksSaltStore::new(dir.path()).unwrap();
                reset_salt_to_pre_pop(&store, &erc20_pre_pop, &pool);
                store.log_bucket_load_stats();
                if iters > 0 {
                    let mut root = SaltStateRoot::new(&store)
                        .with_deferred_levels(3)
                        .with_min_par_batch_size(16);
                    let _ = run_salt_blocks_only(&store, &mut root, &pool, &erc20_block_bundles);
                    reset_salt_to_pre_pop(&store, &erc20_pre_pop, &pool);
                }
                let mut total = Duration::ZERO;
                let mut first_stats: Option<Vec<BlockStats>> = None;
                for i in 0..iters {
                    reset_salt_to_pre_pop(&store, &erc20_pre_pop, &pool);
                    let mut root = SaltStateRoot::new(&store)
                        .with_deferred_levels(3)
                        .with_min_par_batch_size(16);
                    let (elapsed, stats) =
                        run_salt_blocks_only(&store, &mut root, &pool, &erc20_block_bundles);
                    total += elapsed;
                    if i == 0 {
                        first_stats = Some(stats);
                    }
                }
                if let Some(stats) = first_stats {
                    print_blocks_stats(
                        &label_erc20,
                        &format!("SALT({SALT_NUM_THREADS}t)"),
                        &stats,
                        erc20_state_changes_per_block,
                    );
                }
                total
            })
        },
    );

    // ---- Random-account scenario ----
    let label_random = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{ACCOUNTS_PER_BLOCK}accts");
    let state_changes_per_block = ACCOUNTS_PER_BLOCK * (1 + SLOTS_PER_ACCOUNT);
    let pre_pop_entries = PRE_POP_ACCOUNTS * (1 + SLOTS_PER_ACCOUNT);

    let mut rng = StdRng::seed_from_u64(42);
    let pre_pop = generate_bundle_state_random(PRE_POP_ACCOUNTS, SLOTS_PER_ACCOUNT, &mut rng);
    let pre_pop_addresses: Vec<Address> = pre_pop.state().keys().copied().collect();
    assert_eq!(pre_pop_addresses.len(), PRE_POP_ACCOUNTS);

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

    let (random_mpt_sorted, random_mpt_updates, random_setup_elapsed) = prepare_mpt_once(&pre_pop);
    eprintln!(
        "Setup (once): {} accounts, {} entries  →  {:.2?}",
        PRE_POP_ACCOUNTS, pre_pop_entries, random_setup_elapsed
    );

    group.bench_function(BenchmarkId::new("mpt", &label_random), |b| {
        b.iter_custom(|iters| {
            let factory = create_test_provider_factory();
            let mut total = Duration::ZERO;
            let mut first_stats: Option<Vec<BlockStats>> = None;
            if iters > 0 {
                let _ = run_mpt_blocks_only(
                    &factory,
                    &random_mpt_sorted,
                    &random_mpt_updates,
                    &block_bundles,
                    false,
                );
            }
            for i in 0..iters {
                let (elapsed, stats) = run_mpt_blocks_only(
                    &factory,
                    &random_mpt_sorted,
                    &random_mpt_updates,
                    &block_bundles,
                    i == 0,
                );
                total += elapsed;
                if i == 0 {
                    first_stats = Some(stats);
                }
            }
            if let Some(stats) = first_stats {
                print_blocks_stats(&label_random, "MPT", &stats, state_changes_per_block);
            }
            total
        })
    });

    group.bench_function(
        BenchmarkId::new(&format!("salt_{SALT_NUM_THREADS}t"), &label_random),
        |b| {
            b.iter_custom(|iters| {
                let dir = tempfile::TempDir::new().unwrap();
                let store = RocksSaltStore::new(dir.path()).unwrap();
                reset_salt_to_pre_pop(&store, &pre_pop, &pool);
                store.log_bucket_load_stats();
                if iters > 0 {
                    let mut root = SaltStateRoot::new(&store)
                        .with_deferred_levels(3)
                        .with_min_par_batch_size(16);
                    let _ = run_salt_blocks_only(&store, &mut root, &pool, &block_bundles);
                    reset_salt_to_pre_pop(&store, &pre_pop, &pool);
                }
                let mut total = Duration::ZERO;
                let mut first_stats: Option<Vec<BlockStats>> = None;
                for i in 0..iters {
                    reset_salt_to_pre_pop(&store, &pre_pop, &pool);
                    let mut root = SaltStateRoot::new(&store)
                        .with_deferred_levels(3)
                        .with_min_par_batch_size(16);
                    let (elapsed, stats) =
                        run_salt_blocks_only(&store, &mut root, &pool, &block_bundles);
                    total += elapsed;
                    if i == 0 {
                        first_stats = Some(stats);
                    }
                }
                if let Some(stats) = first_stats {
                    print_blocks_stats(
                        &label_random,
                        &format!("SALT({SALT_NUM_THREADS}t)"),
                        &stats,
                        state_changes_per_block,
                    );
                }
                total
            })
        },
    );

    group.finish();
}

criterion_group!(benches, bench_mpt_vs_salt);
criterion_main!(benches);
