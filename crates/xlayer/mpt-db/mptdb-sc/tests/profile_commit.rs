//! Profiling test: detailed timing breakdown of mpt-db commit phases.
//!
//! Run with: cargo test -p mptdb-sc --release --test profile_commit -- --nocapture

use alloy_primitives::{keccak256, map::HashMap as PrimitivesHashMap, Address, B256, U256};
use alloy_trie::KECCAK_EMPTY;
use mptdb_sc::mpt::{BulkLoadOptions, CommitProfile, MptCommitStore, MptCommitter};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::time::Instant;
use tempfile::TempDir;

const PREPOP_CHUNK_SIZE: usize = 10_000;

fn wal_first_config() -> mptdb_sc::mpt::MptConfig {
    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.wal_first_commit = true;
    config.checkpoint_max_account_trie_nodes = 0;
    config
}

fn generate_addresses(num: usize, rng: &mut StdRng) -> Vec<Address> {
    let mut addresses = Vec::with_capacity(num);
    let mut addr_buf = [0u8; 20];

    for _ in 0..num {
        rng.fill_bytes(&mut addr_buf);
        let addr = Address::from(addr_buf);
        addresses.push(addr);
    }

    addresses
}

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

fn generate_accounts(
    num: usize,
    slots_per: usize,
    rng: &mut StdRng,
) -> (revm_database::BundleState, Vec<Address>) {
    let addresses = generate_addresses(num, rng);
    let bundle = generate_account_chunk(&addresses, 0, slots_per);
    (bundle, addresses)
}

fn generate_updates(
    addresses: &[Address],
    count: usize,
    slots_per: usize,
    block_idx: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for i in 0..count {
        let idx = rng.random_range(0..addresses.len());
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

fn prepopulate_store_in_chunks(
    store: &mut MptCommitStore,
    addresses: &[Address],
    slots_per: usize,
    chunk_size: usize,
) {
    store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
    for (chunk_idx, chunk) in addresses.chunks(chunk_size).enumerate() {
        let start_index = chunk_idx * chunk_size;
        let bundle = generate_account_chunk(chunk, start_index, slots_per);
        store.bulk_ingest_bundle_chunk(&bundle).unwrap();
    }
    store.finish_bulk_load().unwrap();
}

/// Profile apply_bundle_state breakdown: how much time is spent in
/// collect_dirty_accounts vs storage trie loading/modification.
///
/// Since we can't instrument library internals from a test, we measure
/// the BundleState→DirtyAccount conversion separately by calling keccak256
/// on all addresses+slots (which is what collect_dirty_accounts does).
#[test]
#[ignore]
fn profile_b4_2_breakdown() {
    let mut rng = StdRng::seed_from_u64(4200);
    let (pre_pop, addrs) = generate_accounts(1_000, 10, &mut rng);

    let mut rng_block = StdRng::seed_from_u64(4201);
    let block = generate_updates(&addrs, 200, 10, 0, &mut rng_block);

    // Measure the cost of keccak256 hashing (what collect_dirty_accounts does)
    let t_hash = Instant::now();
    for _ in 0..200 {
        for (addr, acct) in block.state() {
            let _ = keccak256(addr);
            for (slot, _) in &acct.storage {
                let _ = keccak256(B256::from(*slot));
            }
        }
    }
    let hash_us = t_hash.elapsed().as_micros() / 200;

    // Count total keccak256 calls per block
    let mut num_hash = 0usize;
    for (_addr, acct) in block.state() {
        num_hash += 1; // address hash
        num_hash += acct.storage.len(); // slot hashes
    }

    println!("\n=== B4.2 Breakdown ===");
    println!("Unique accounts in block: {}", block.state().len());
    println!("Total keccak256 calls per apply: {num_hash}");
    println!("keccak256 cost: {hash_us}µs ({num_hash} hashes)");

    // Now profile the full pipeline
    let iterations = 50;
    let mut apply_times = Vec::new();
    let mut commit_times = Vec::new();

    for _ in 0..iterations {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        store.apply_bundle_state(&pre_pop).unwrap();
        store.commit().unwrap();

        let t0 = Instant::now();
        store.apply_bundle_state(&block).unwrap();
        apply_times.push(t0.elapsed().as_micros());

        let ((_version, _root), profile) = store.commit_with_profile().unwrap();
        commit_times.push(profile.total_commit.as_micros());

        store.close().unwrap();
    }

    apply_times.sort();
    commit_times.sort();

    let p50 = |v: &[u128]| v[v.len() / 2];
    let p10 = |v: &[u128]| v[v.len() / 10];

    println!("apply_bundle_state: p10={}µs p50={}µs", p10(&apply_times), p50(&apply_times));
    println!("commit:             p10={}µs p50={}µs", p10(&commit_times), p50(&commit_times));
    println!(
        "total:              p10={}µs p50={}µs",
        p10(&apply_times) + p10(&commit_times),
        p50(&apply_times) + p50(&commit_times)
    );

    // Also profile what reth-equivalent hashing would cost
    // (BundleState → HashedPostState is essentially keccak256 on all keys)
    println!("\nFor reference:");
    println!("  keccak256 per address: ~{}ns", {
        let t = Instant::now();
        let dummy = [0u8; 20];
        for _ in 0..100_000 {
            let _ = keccak256(dummy);
        }
        t.elapsed().as_nanos() / 100_000
    });
    println!("  keccak256 per slot:    ~{}ns", {
        let t = Instant::now();
        let dummy = [0u8; 32];
        for _ in 0..100_000 {
            let _ = keccak256(dummy);
        }
        t.elapsed().as_nanos() / 100_000
    });
}

/// Profile B4.3 with per-block breakdown.
#[test]
#[ignore]
fn profile_b4_3_per_block() {
    let mut rng = StdRng::seed_from_u64(4300);
    let (pre_pop, addrs) = generate_accounts(1_000, 10, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4301);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 200, 10, i, &mut rng_blocks)).collect();

    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    store.apply_bundle_state(&pre_pop).unwrap();
    store.commit().unwrap();

    println!("\n=== B4.3 Per-Block Profile (release mode) ===");
    println!("{:<8} {:<12} {:<12} {:<12}", "Block", "Apply(µs)", "Commit(µs)", "Total(µs)");

    let mut total_apply = 0u128;
    let mut total_commit = 0u128;

    for (i, block) in blocks.iter().enumerate() {
        let t0 = Instant::now();
        store.apply_bundle_state(block).unwrap();
        let apply_us = t0.elapsed().as_micros();

        let ((_version, _root), profile) = store.commit_with_profile().unwrap();
        let commit_us = profile.total_commit.as_micros();

        total_apply += apply_us;
        total_commit += commit_us;

        println!("{:<8} {:<12} {:<12} {:<12}", i + 1, apply_us, commit_us, apply_us + commit_us);
    }

    println!(
        "{:<8} {:<12} {:<12} {:<12}",
        "TOTAL",
        total_apply,
        total_commit,
        total_apply + total_commit
    );
    println!(
        "{:<8} {:<12} {:<12} {:<12}",
        "AVG",
        total_apply / 10,
        total_commit / 10,
        (total_apply + total_commit) / 10
    );

    store.close().unwrap();
}

#[test]
#[ignore]
fn profile_commit_phases_structured() {
    let mut rng = StdRng::seed_from_u64(5200);
    let (pre_pop, addrs) = generate_accounts(1_000, 10, &mut rng);

    let mut rng_block = StdRng::seed_from_u64(5201);
    let block = generate_updates(&addrs, 200, 10, 0, &mut rng_block);

    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();
    store.apply_bundle_state(&pre_pop).unwrap();
    store.commit().unwrap();

    store.apply_bundle_state(&block).unwrap();
    let ((_version, _root), profile) = store.commit_with_profile().unwrap();
    print_profile("B4.2 single-block commit profile", &profile);
    store.close().unwrap();
}

#[test]
#[ignore]
fn profile_reopen_from_checkpoint_baseline() {
    let mut rng = StdRng::seed_from_u64(5300);
    let (pre_pop, _addrs) = generate_accounts(10_000, 4, &mut rng);

    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();
    store.apply_bundle_state(&pre_pop).unwrap();
    store.commit().unwrap();
    store.close().unwrap();

    let t = Instant::now();
    let reopened = MptCommitStore::open(dir.path(), false).unwrap();
    let reopen_ms = t.elapsed().as_millis();

    println!("\n=== Reopen Checkpoint Baseline ===");
    println!("reopen from checkpoint-backed account trie: {reopen_ms} ms");
    println!("frontier: {:?}", reopened.frontier());
}

fn print_profile(label: &str, profile: &CommitProfile) {
    println!("\n=== {label} ===");
    println!("apply_bundle_state:     {:>8} µs", profile.apply_bundle_state.as_micros());
    println!("  collect_dirty:        {:>8} µs", profile.apply_collect_dirty_accounts.as_micros());
    println!(
        "  get_or_load_trie:     {:>8} µs",
        profile.apply_get_or_load_storage_tries.as_micros()
    );
    println!("  slot_updates:         {:>8} µs", profile.apply_storage_slot_updates.as_micros());
    println!("  l3_latest_load:       {:>8} µs", profile.apply_l3_latest_load.as_micros());
    println!("  l3_published_load:    {:>8} µs", profile.apply_l3_published_load.as_micros());
    println!("  l3_into_tree:         {:>8} µs", profile.apply_l3_into_tree.as_micros());
    println!("  published_refreshes:  {:>8}", profile.apply_published_refreshes);
    println!("  l2_hits:              {:>8}", profile.apply_l2_hits);
    println!("  l3_latest_hits:       {:>8}", profile.apply_l3_latest_hits);
    println!("  l3_published_hits:    {:>8}", profile.apply_l3_published_hits);
    println!("  l3_published_post:    {:>8}", profile.apply_l3_published_post_flush_hits);
    println!("  node_fallback_loads:  {:>8}", profile.apply_node_fallback_loads);
    println!("  slot_inserts:         {:>8}", profile.apply_slot_inserts);
    println!("  slot_deletes:         {:>8}", profile.apply_slot_deletes);
    println!("  leaf_splits:          {:>8}", profile.apply_leaf_splits);
    println!("  ext_splits:           {:>8}", profile.apply_extension_splits);
    println!("  br_collapse_empty:    {:>8}", profile.apply_branch_collapse_to_empty);
    println!("  br_collapse_leaf:     {:>8}", profile.apply_branch_collapse_to_leaf);
    println!("  br_collapse_ext:      {:>8}", profile.apply_branch_collapse_to_extension);
    println!("  ext_leaf_merges:      {:>8}", profile.apply_extension_leaf_merges);
    println!("  ext_ext_merges:       {:>8}", profile.apply_extension_extension_merges);
    println!("storage_roots:          {:>8} µs", profile.storage_roots.as_micros());
    println!("  root_hashing:         {:>8} µs", profile.storage_root_hashing.as_micros());
    println!("  segment_build:        {:>8} µs", profile.storage_segment_build.as_micros());
    println!("account_updates:        {:>8} µs", profile.account_updates.as_micros());
    println!("account_root_and_blobs: {:>8} µs", profile.account_root_and_blobs.as_micros());
    println!("wal_append:             {:>8} µs", profile.wal_append.as_micros());
    println!("wal_replay:             {:>8} µs", profile.wal_replay.as_micros());
    println!("durable_materialize:    {:>8} µs", profile.durable_materialize.as_micros());
    println!("published_materialize:  {:>8} µs", profile.published_materialize.as_micros());
    println!("durable_version_lag:    {:>8}", profile.durable_version_lag);
    println!("published_version_lag:  {:>8}", profile.published_version_lag);
    println!("persist_and_manifest:   {:>8} µs", profile.persist_and_manifest.as_micros());
    println!("  persist_batch:        {:>8} µs", profile.persist_batch.as_micros());
    println!("  manifest_save:        {:>8} µs", profile.manifest_save.as_micros());
    println!("  publish_generation:   {:>8} µs", profile.publish_generation.as_micros());
    println!("  open_published_store: {:>8} µs", profile.open_published_store.as_micros());
    println!("cache_publish:          {:>8} µs", profile.cache_publish.as_micros());
    println!("total_commit:           {:>8} µs", profile.total_commit.as_micros());
}

/// Profile B4.4 large scale.
#[test]
#[ignore]
fn profile_b4_4_large() {
    let mut rng = StdRng::seed_from_u64(4400);
    let (pre_pop, addrs) = generate_accounts(200_000, 10, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4401);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 2_000, 10, i, &mut rng_blocks)).collect();

    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();

    let t0 = Instant::now();
    store.apply_bundle_state(&pre_pop).unwrap();
    println!("\n=== B4.4 Profile (200K pre-pop, 10 x 2K updates) ===");
    println!("Pre-pop apply: {}ms", t0.elapsed().as_millis());
    let t1 = Instant::now();
    store.commit().unwrap();
    println!("Pre-pop commit: {}ms", t1.elapsed().as_millis());

    println!(
        "{:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10}",
        "Block",
        "Apply",
        "Collect",
        "TrieLd",
        "SlotUpd",
        "LtLd",
        "PbLd",
        "ToTree",
        "L2Hit",
        "L3Hit",
        "NDFall",
        "StorRoot",
        "RtHash",
        "SegBld",
        "AcctUpd",
        "AcctRoot",
        "Persist",
        "Commit",
        "Total",
        "Ins/Del",
        "Split/Mrg"
    );

    for (i, block) in blocks.iter().enumerate() {
        store.apply_bundle_state(block).unwrap();
        let ((_version, _root), profile) = store.commit_with_profile().unwrap();

        println!(
            "{:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10}",
            i + 1,
            profile.apply_bundle_state.as_millis(),
            profile.apply_collect_dirty_accounts.as_millis(),
            profile.apply_get_or_load_storage_tries.as_millis(),
            profile.apply_storage_slot_updates.as_millis(),
            profile.apply_l3_latest_load.as_millis(),
            profile.apply_l3_published_load.as_millis(),
            profile.apply_l3_into_tree.as_millis(),
            profile.apply_l2_hits,
            profile.apply_l3_latest_hits
                + profile.apply_l3_published_hits
                + profile.apply_l3_published_post_flush_hits,
            profile.apply_node_fallback_loads,
            profile.storage_roots.as_millis(),
            profile.storage_root_hashing.as_millis(),
            profile.storage_segment_build.as_millis(),
            profile.account_updates.as_millis(),
            profile.account_root_and_blobs.as_millis(),
            profile.persist_and_manifest.as_millis(),
            profile.total_commit.as_millis(),
            (profile.apply_bundle_state + profile.total_commit).as_millis(),
            format!("{}/{}", profile.apply_slot_inserts, profile.apply_slot_deletes),
            format!(
                "{}/{}/{}/{}",
                profile.apply_leaf_splits + profile.apply_extension_splits,
                profile.apply_branch_collapse_to_empty
                    + profile.apply_branch_collapse_to_leaf
                    + profile.apply_branch_collapse_to_extension,
                profile.apply_extension_leaf_merges,
                profile.apply_extension_extension_merges
            )
        );
    }

    store.close().unwrap();
}

/// Helper: print the per-block profile table header + rows for large-scale profiles.
fn print_large_profile_table(
    label: &str,
    store: &mut MptCommitStore,
    addresses: &[Address],
    slots_per: usize,
    blocks: &[revm_database::BundleState],
) {
    let t0 = Instant::now();
    prepopulate_store_in_chunks(store, addresses, slots_per, PREPOP_CHUNK_SIZE);
    println!("\n=== {label} ===");
    println!("Pre-pop total: {}ms", t0.elapsed().as_millis());
    println!(
        "Pre-pop chunks: {} x {}",
        addresses.len().div_ceil(PREPOP_CHUNK_SIZE),
        PREPOP_CHUNK_SIZE
    );

    println!(
        "{:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8} {:<8} {:<8} {:<8} {:<7} {:<7} {:<10} {:<10} {:<10}",
        "Block", "Apply", "Collect", "TrieLd", "SlotUpd", "LtLd", "PbLd", "ToTree",
        "L2Hit", "L3Hit", "NDFall", "StorRoot", "RtHash", "SegBld", "AcctUpd",
        "AcctRoot", "WalApp", "WalRpl", "DurMat", "PubMat", "DLag", "PLag", "Persist",
        "Commit", "Total"
    );

    for (i, block) in blocks.iter().enumerate() {
        store.apply_bundle_state(block).unwrap();
        let ((_version, _root), profile) = store.commit_with_profile().unwrap();

        println!(
            "{:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8} {:<8} {:<8} {:<8} {:<7} {:<7} {:<10} {:<10} {:<10}",
            i + 1,
            profile.apply_bundle_state.as_millis(),
            profile.apply_collect_dirty_accounts.as_millis(),
            profile.apply_get_or_load_storage_tries.as_millis(),
            profile.apply_storage_slot_updates.as_millis(),
            profile.apply_l3_latest_load.as_millis(),
            profile.apply_l3_published_load.as_millis(),
            profile.apply_l3_into_tree.as_millis(),
            profile.apply_l2_hits,
            profile.apply_l3_latest_hits
                + profile.apply_l3_published_hits
                + profile.apply_l3_published_post_flush_hits,
            profile.apply_node_fallback_loads,
            profile.storage_roots.as_millis(),
            profile.storage_root_hashing.as_millis(),
            profile.storage_segment_build.as_millis(),
            profile.account_updates.as_millis(),
            profile.account_root_and_blobs.as_millis(),
            profile.wal_append.as_millis(),
            profile.wal_replay.as_millis(),
            profile.durable_materialize.as_millis(),
            profile.published_materialize.as_millis(),
            profile.durable_version_lag,
            profile.published_version_lag,
            profile.persist_and_manifest.as_millis(),
            profile.total_commit.as_millis(),
            (profile.apply_bundle_state + profile.total_commit).as_millis(),
        );
    }
}

/// Profile B4.5 near-production: 1M accounts × 10 slots, 10 blocks × 5K updates.
#[test]
#[ignore]
fn profile_b4_5_near_production() {
    let mut rng = StdRng::seed_from_u64(4500);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4501);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 5_000, 10, i, &mut rng_blocks)).collect();

    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();

    print_large_profile_table(
        "B4.5 Profile (1M pre-pop × 10 slots, 10 x 5K updates)",
        &mut store,
        &addrs,
        10,
        &blocks,
    );

    if std::env::var_os("MPT_DEBUG_SKIP_CLOSE").is_some() {
        std::mem::forget(store);
        return;
    }
    store.close().unwrap();
}

/// Profile B4.6 storage-heavy large: 1M accounts × 30 slots, 10 blocks × 10K updates.
#[test]
#[ignore]
fn profile_b4_6_storage_heavy_large() {
    let mut rng = StdRng::seed_from_u64(4600);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4601);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 10_000, 30, i, &mut rng_blocks)).collect();

    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    print_large_profile_table(
        "B4.6 Profile (1M pre-pop × 30 slots, 10 x 10K updates)",
        &mut store,
        &addrs,
        30,
        &blocks,
    );

    if std::env::var_os("MPT_DEBUG_SKIP_CLOSE").is_some() {
        std::mem::forget(store);
        return;
    }
    store.close().unwrap();
}
