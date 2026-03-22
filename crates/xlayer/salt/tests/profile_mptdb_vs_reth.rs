//! Lightweight one-shot profile for mpt-db vs reth on the shared B4.5 dataset.
//!
//! Run with:
//! `PROTOC=/Users/louisliuxiong/golang/bin/protoc cargo test -p xlayer-salt --release --test
//! profile_mptdb_vs_reth profile_b4_5_single_run_compare -- --ignored --nocapture`

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use mptdb_sc::mpt::{BulkLoadOptions, CommitProfile, MptCommitStore, MptCommitter};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use reth_provider::{test_utils::create_test_provider_factory, StateWriter, TrieWriter};
use reth_trie::{updates::TrieUpdates, HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::time::{Duration, Instant};
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

fn prepopulate_mptdb_in_chunks(
    store: &mut MptCommitStore,
    addresses: &[Address],
    slots_per: usize,
    chunk_size: usize,
) -> Duration {
    let start = Instant::now();
    store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
    for (chunk_idx, chunk) in addresses.chunks(chunk_size).enumerate() {
        let start_index = chunk_idx * chunk_size;
        let bundle = generate_account_chunk(chunk, start_index, slots_per);
        let _ = store.bulk_ingest_bundle_chunk(&bundle).unwrap();
    }
    store.finish_bulk_load().unwrap();
    start.elapsed()
}

#[derive(Default)]
struct RethBlockProfile {
    hash_and_sort: Duration,
    root_updates: Duration,
    write_hashed: Duration,
    write_trie: Duration,
    commit: Duration,
    total: Duration,
}

#[derive(Default)]
struct MptdbTotals {
    apply: Duration,
    trie_load: Duration,
    slot_updates: Duration,
    l3_latest: Duration,
    l3_published: Duration,
    to_tree: Duration,
    commit: Duration,
    storage_roots: Duration,
    account_updates: Duration,
    account_root: Duration,
    wal_append: Duration,
    wal_replay: Duration,
    durable_materialize: Duration,
    published_materialize: Duration,
    persist: Duration,
    persist_batch: Duration,
    manifest_save: Duration,
    publish_generation: Duration,
    open_published_store: Duration,
    cache_publish: Duration,
    storage_segment_build: Duration,
    storage_root_hashing: Duration,
    durable_version_lag: i64,
    published_version_lag: i64,
    l2_hits: u64,
    l3_hits: u64,
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

#[test]
#[ignore]
fn profile_b4_4_single_run_compare() {
    let mut rng = StdRng::seed_from_u64(4400);
    let addrs = generate_addresses(200_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4401);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 2_000, 10, i, &mut rng_blocks)).collect();

    println!("\n=== B4.4 Single-Run Compare ===");
    println!("Dataset: 200K pre-pop x 10 slots, 10 blocks x 2K updates");
    println!(
        "Pre-pop mode: {} chunks x {} accounts",
        addrs.len().div_ceil(PREPOP_CHUNK_SIZE),
        PREPOP_CHUNK_SIZE
    );

    let factory = create_test_provider_factory();

    let reth_prepop = {
        let start = Instant::now();
        for (chunk_idx, chunk) in addrs.chunks(PREPOP_CHUNK_SIZE).enumerate() {
            let start_index = chunk_idx * PREPOP_CHUNK_SIZE;
            let bundle = generate_account_chunk(chunk, start_index, 10);
            let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
            let sorted = hashed.into_sorted();
            let rw = factory.provider_rw().unwrap();
            rw.write_hashed_state(&sorted).unwrap();
            let (_, updates): (B256, TrieUpdates) =
                StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
            rw.write_trie_updates(updates).unwrap();
            rw.commit().unwrap();
        }
        start.elapsed()
    };

    let mut reth_totals = RethBlockProfile::default();
    for block in &blocks {
        let total_start = Instant::now();

        let hash_start = Instant::now();
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(block.state());
        let sorted = hashed.into_sorted();
        reth_totals.hash_and_sort += hash_start.elapsed();

        let rw = factory.provider_rw().unwrap();

        let root_start = Instant::now();
        let (_, updates) = StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted).unwrap();
        reth_totals.root_updates += root_start.elapsed();

        let write_hashed_start = Instant::now();
        rw.write_hashed_state(&sorted).unwrap();
        reth_totals.write_hashed += write_hashed_start.elapsed();

        let write_trie_start = Instant::now();
        rw.write_trie_updates(updates).unwrap();
        reth_totals.write_trie += write_trie_start.elapsed();

        let commit_start = Instant::now();
        rw.commit().unwrap();
        reth_totals.commit += commit_start.elapsed();

        reth_totals.total += total_start.elapsed();
    }

    drop(factory);

    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();

    let mptdb_prepop = prepopulate_mptdb_in_chunks(&mut store, &addrs, 10, PREPOP_CHUNK_SIZE);

    let mut mptdb_totals = MptdbTotals::default();
    for block in &blocks {
        let apply_start = Instant::now();
        store.apply_bundle_state(block).unwrap();
        mptdb_totals.apply += apply_start.elapsed();

        let ((_version, _root), profile): ((i64, B256), CommitProfile) =
            store.commit_with_profile().unwrap();
        mptdb_totals.trie_load += profile.apply_get_or_load_storage_tries;
        mptdb_totals.slot_updates += profile.apply_storage_slot_updates;
        mptdb_totals.l3_latest += profile.apply_l3_latest_load;
        mptdb_totals.l3_published += profile.apply_l3_published_load;
        mptdb_totals.to_tree += profile.apply_l3_into_tree;
        mptdb_totals.commit += profile.total_commit;
        mptdb_totals.storage_roots += profile.storage_roots;
        mptdb_totals.account_updates += profile.account_updates;
        mptdb_totals.account_root += profile.account_root_and_blobs;
        mptdb_totals.wal_append += profile.wal_append;
        mptdb_totals.wal_replay += profile.wal_replay;
        mptdb_totals.durable_materialize += profile.durable_materialize;
        mptdb_totals.published_materialize += profile.published_materialize;
        mptdb_totals.persist += profile.persist_and_manifest;
        mptdb_totals.persist_batch += profile.persist_batch;
        mptdb_totals.manifest_save += profile.manifest_save;
        mptdb_totals.publish_generation += profile.publish_generation;
        mptdb_totals.open_published_store += profile.open_published_store;
        mptdb_totals.cache_publish += profile.cache_publish;
        mptdb_totals.storage_segment_build += profile.storage_segment_build;
        mptdb_totals.storage_root_hashing += profile.storage_root_hashing;
        mptdb_totals.durable_version_lag += profile.durable_version_lag;
        mptdb_totals.published_version_lag += profile.published_version_lag;
        mptdb_totals.l2_hits += profile.apply_l2_hits;
        mptdb_totals.l3_hits += profile.apply_l3_latest_hits +
            profile.apply_l3_published_hits +
            profile.apply_l3_published_post_flush_hits;
    }
    store.close().unwrap();

    let blocks_len = blocks.len() as u32;

    println!("\nreth");
    println!("  pre-pop total:       {} ms", fmt_ms(reth_prepop));
    println!("  per-block total:     {} ms", fmt_ms(reth_totals.total / blocks_len));
    println!("  hash+sort:           {} ms", fmt_ms(reth_totals.hash_and_sort / blocks_len));
    println!("  root_with_updates:   {} ms", fmt_ms(reth_totals.root_updates / blocks_len));
    println!("  write_hashed_state:  {} ms", fmt_ms(reth_totals.write_hashed / blocks_len));
    println!("  write_trie_updates:  {} ms", fmt_ms(reth_totals.write_trie / blocks_len));
    println!("  commit:              {} ms", fmt_ms(reth_totals.commit / blocks_len));

    println!("\nmpt-db");
    println!("  pre-pop total:       {} ms", fmt_ms(mptdb_prepop));
    println!(
        "  per-block total:     {} ms",
        fmt_ms((mptdb_totals.apply + mptdb_totals.commit) / blocks_len)
    );
    println!("  apply_bundle_state:  {} ms", fmt_ms(mptdb_totals.apply / blocks_len));
    println!("  trie_load:           {} ms", fmt_ms(mptdb_totals.trie_load / blocks_len));
    println!("  slot_updates:        {} ms", fmt_ms(mptdb_totals.slot_updates / blocks_len));
    println!("  commit:              {} ms", fmt_ms(mptdb_totals.commit / blocks_len));
    println!("  storage_roots:       {} ms", fmt_ms(mptdb_totals.storage_roots / blocks_len));
    println!("  account_updates:     {} ms", fmt_ms(mptdb_totals.account_updates / blocks_len));
    println!("  account_root:        {} ms", fmt_ms(mptdb_totals.account_root / blocks_len));
    println!("  wal_append:          {} ms", fmt_ms(mptdb_totals.wal_append / blocks_len));
    println!(
        "  segment_build:       {} ms",
        fmt_ms(mptdb_totals.storage_segment_build / blocks_len)
    );
    println!(
        "  root_hashing:        {} ms",
        fmt_ms(mptdb_totals.storage_root_hashing / blocks_len)
    );
    println!(
        "  avg hits/block:      L2={}, L3={}",
        mptdb_totals.l2_hits / blocks.len() as u64,
        mptdb_totals.l3_hits / blocks.len() as u64
    );

    let reth_avg = reth_totals.total / blocks_len;
    let mptdb_avg = (mptdb_totals.apply + mptdb_totals.commit) / blocks_len;
    println!(
        "\napprox ratio (single run): mpt-db / reth = {:.2}x",
        mptdb_avg.as_secs_f64() / reth_avg.as_secs_f64()
    );
}

#[test]
#[ignore]
fn profile_b4_5_single_run_compare() {
    let mut rng = StdRng::seed_from_u64(4500);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4501);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 5_000, 10, i, &mut rng_blocks)).collect();

    println!("\n=== B4.5 Single-Run Compare ===");
    println!("Dataset: 1M pre-pop x 10 slots, 10 blocks x 5K updates");
    println!(
        "Pre-pop mode: {} chunks x {} accounts",
        addrs.len().div_ceil(PREPOP_CHUNK_SIZE),
        PREPOP_CHUNK_SIZE
    );

    let factory = create_test_provider_factory();

    let reth_prepop = {
        let start = Instant::now();
        for (chunk_idx, chunk) in addrs.chunks(PREPOP_CHUNK_SIZE).enumerate() {
            let start_index = chunk_idx * PREPOP_CHUNK_SIZE;
            let bundle = generate_account_chunk(chunk, start_index, 10);
            let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
            let sorted = hashed.into_sorted();
            let rw = factory.provider_rw().unwrap();
            rw.write_hashed_state(&sorted).unwrap();
            let (_, updates): (B256, TrieUpdates) =
                StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
            rw.write_trie_updates(updates).unwrap();
            rw.commit().unwrap();
        }
        start.elapsed()
    };

    let mut reth_totals = RethBlockProfile::default();
    for block in &blocks {
        let total_start = Instant::now();

        let hash_start = Instant::now();
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(block.state());
        let sorted = hashed.into_sorted();
        reth_totals.hash_and_sort += hash_start.elapsed();

        let rw = factory.provider_rw().unwrap();

        let root_start = Instant::now();
        let (_, updates) = StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted).unwrap();
        reth_totals.root_updates += root_start.elapsed();

        let write_hashed_start = Instant::now();
        rw.write_hashed_state(&sorted).unwrap();
        reth_totals.write_hashed += write_hashed_start.elapsed();

        let write_trie_start = Instant::now();
        rw.write_trie_updates(updates).unwrap();
        reth_totals.write_trie += write_trie_start.elapsed();

        let commit_start = Instant::now();
        rw.commit().unwrap();
        reth_totals.commit += commit_start.elapsed();

        reth_totals.total += total_start.elapsed();
    }

    // Keep the compare fair: release reth's provider/db state before measuring
    // mpt-db so the second half is not distorted by extra memory pressure.
    drop(factory);

    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();

    let mptdb_prepop = prepopulate_mptdb_in_chunks(&mut store, &addrs, 10, PREPOP_CHUNK_SIZE);

    let mut mptdb_totals = MptdbTotals::default();
    for block in &blocks {
        let apply_start = Instant::now();
        store.apply_bundle_state(block).unwrap();
        mptdb_totals.apply += apply_start.elapsed();

        let ((_version, _root), profile): ((i64, B256), CommitProfile) =
            store.commit_with_profile().unwrap();
        mptdb_totals.trie_load += profile.apply_get_or_load_storage_tries;
        mptdb_totals.slot_updates += profile.apply_storage_slot_updates;
        mptdb_totals.l3_latest += profile.apply_l3_latest_load;
        mptdb_totals.l3_published += profile.apply_l3_published_load;
        mptdb_totals.to_tree += profile.apply_l3_into_tree;
        mptdb_totals.commit += profile.total_commit;
        mptdb_totals.storage_roots += profile.storage_roots;
        mptdb_totals.account_updates += profile.account_updates;
        mptdb_totals.account_root += profile.account_root_and_blobs;
        mptdb_totals.wal_append += profile.wal_append;
        mptdb_totals.wal_replay += profile.wal_replay;
        mptdb_totals.durable_materialize += profile.durable_materialize;
        mptdb_totals.published_materialize += profile.published_materialize;
        mptdb_totals.persist += profile.persist_and_manifest;
        mptdb_totals.persist_batch += profile.persist_batch;
        mptdb_totals.manifest_save += profile.manifest_save;
        mptdb_totals.publish_generation += profile.publish_generation;
        mptdb_totals.open_published_store += profile.open_published_store;
        mptdb_totals.cache_publish += profile.cache_publish;
        mptdb_totals.storage_segment_build += profile.storage_segment_build;
        mptdb_totals.storage_root_hashing += profile.storage_root_hashing;
        mptdb_totals.durable_version_lag += profile.durable_version_lag;
        mptdb_totals.published_version_lag += profile.published_version_lag;
        mptdb_totals.l2_hits += profile.apply_l2_hits;
        mptdb_totals.l3_hits += profile.apply_l3_latest_hits +
            profile.apply_l3_published_hits +
            profile.apply_l3_published_post_flush_hits;
    }
    store.close().unwrap();

    let blocks_len = blocks.len() as u32;

    println!("\nreth");
    println!("  pre-pop total:       {} ms", fmt_ms(reth_prepop));
    println!("  per-block total:     {} ms", fmt_ms(reth_totals.total / blocks_len));
    println!("  hash+sort:           {} ms", fmt_ms(reth_totals.hash_and_sort / blocks_len));
    println!("  root_with_updates:   {} ms", fmt_ms(reth_totals.root_updates / blocks_len));
    println!("  write_hashed_state:  {} ms", fmt_ms(reth_totals.write_hashed / blocks_len));
    println!("  write_trie_updates:  {} ms", fmt_ms(reth_totals.write_trie / blocks_len));
    println!("  commit:              {} ms", fmt_ms(reth_totals.commit / blocks_len));

    println!("\nmpt-db");
    println!("  pre-pop total:       {} ms", fmt_ms(mptdb_prepop));
    println!(
        "  per-block total:     {} ms",
        fmt_ms((mptdb_totals.apply + mptdb_totals.commit) / blocks_len)
    );
    println!("  apply_bundle_state:  {} ms", fmt_ms(mptdb_totals.apply / blocks_len));
    println!("  trie_load:           {} ms", fmt_ms(mptdb_totals.trie_load / blocks_len));
    println!("  l3_latest_lookup:    {} ms", fmt_ms(mptdb_totals.l3_latest / blocks_len));
    println!("  l3_published_lookup: {} ms", fmt_ms(mptdb_totals.l3_published / blocks_len));
    println!("  l3_into_tree:        {} ms", fmt_ms(mptdb_totals.to_tree / blocks_len));
    println!("  slot_updates:        {} ms", fmt_ms(mptdb_totals.slot_updates / blocks_len));
    println!("  commit:              {} ms", fmt_ms(mptdb_totals.commit / blocks_len));
    println!("  storage_roots:       {} ms", fmt_ms(mptdb_totals.storage_roots / blocks_len));
    println!("  account_updates:     {} ms", fmt_ms(mptdb_totals.account_updates / blocks_len));
    println!("  account_root:        {} ms", fmt_ms(mptdb_totals.account_root / blocks_len));
    println!("  wal_append:          {} ms", fmt_ms(mptdb_totals.wal_append / blocks_len));
    println!("  wal_replay:          {} ms", fmt_ms(mptdb_totals.wal_replay / blocks_len));
    println!("  durable_materialize: {} ms", fmt_ms(mptdb_totals.durable_materialize / blocks_len));
    println!(
        "  published_materialize:{} ms",
        fmt_ms(mptdb_totals.published_materialize / blocks_len)
    );
    println!("  persist:             {} ms", fmt_ms(mptdb_totals.persist / blocks_len));
    println!("    persist_batch:     {} ms", fmt_ms(mptdb_totals.persist_batch / blocks_len));
    println!("    manifest_save:     {} ms", fmt_ms(mptdb_totals.manifest_save / blocks_len));
    println!("    publish_generation:{} ms", fmt_ms(mptdb_totals.publish_generation / blocks_len));
    println!(
        "    open_published:    {} ms",
        fmt_ms(mptdb_totals.open_published_store / blocks_len)
    );
    println!("  cache_publish:       {} ms", fmt_ms(mptdb_totals.cache_publish / blocks_len));
    println!(
        "  segment_build:       {} ms",
        fmt_ms(mptdb_totals.storage_segment_build / blocks_len)
    );
    println!(
        "  root_hashing:        {} ms",
        fmt_ms(mptdb_totals.storage_root_hashing / blocks_len)
    );
    println!(
        "  avg hits/block:      L2={}, L3={}",
        mptdb_totals.l2_hits / blocks.len() as u64,
        mptdb_totals.l3_hits / blocks.len() as u64
    );
    println!(
        "  avg version lag:     durable={:.1}, published={:.1}",
        mptdb_totals.durable_version_lag as f64 / blocks.len() as f64,
        mptdb_totals.published_version_lag as f64 / blocks.len() as f64
    );

    let reth_avg = reth_totals.total / blocks_len;
    let mptdb_avg = (mptdb_totals.apply + mptdb_totals.commit) / blocks_len;
    println!(
        "\napprox ratio (single run): mpt-db / reth = {:.2}x",
        mptdb_avg.as_secs_f64() / reth_avg.as_secs_f64()
    );
}

#[test]
#[ignore]
fn profile_b4_6_single_run_compare() {
    let mut rng = StdRng::seed_from_u64(4600);
    let addrs = generate_addresses(1_000_000, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4601);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 10_000, 30, i, &mut rng_blocks)).collect();

    println!("\n=== B4.6 Single-Run Compare ===");
    println!("Dataset: 1M pre-pop x 30 slots, 10 blocks x 10K updates");
    println!(
        "Pre-pop mode: {} chunks x {} accounts",
        addrs.len().div_ceil(PREPOP_CHUNK_SIZE),
        PREPOP_CHUNK_SIZE
    );

    let factory = create_test_provider_factory();

    let reth_prepop = {
        let start = Instant::now();
        for (chunk_idx, chunk) in addrs.chunks(PREPOP_CHUNK_SIZE).enumerate() {
            let start_index = chunk_idx * PREPOP_CHUNK_SIZE;
            let bundle = generate_account_chunk(chunk, start_index, 30);
            let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
            let sorted = hashed.into_sorted();
            let rw = factory.provider_rw().unwrap();
            rw.write_hashed_state(&sorted).unwrap();
            let (_, updates): (B256, TrieUpdates) =
                StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
            rw.write_trie_updates(updates).unwrap();
            rw.commit().unwrap();
        }
        start.elapsed()
    };

    let mut reth_totals = RethBlockProfile::default();
    for block in &blocks {
        let total_start = Instant::now();

        let hash_start = Instant::now();
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(block.state());
        let sorted = hashed.into_sorted();
        reth_totals.hash_and_sort += hash_start.elapsed();

        let rw = factory.provider_rw().unwrap();

        let root_start = Instant::now();
        let (_, updates) = StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted).unwrap();
        reth_totals.root_updates += root_start.elapsed();

        let write_hashed_start = Instant::now();
        rw.write_hashed_state(&sorted).unwrap();
        reth_totals.write_hashed += write_hashed_start.elapsed();

        let write_trie_start = Instant::now();
        rw.write_trie_updates(updates).unwrap();
        reth_totals.write_trie += write_trie_start.elapsed();

        let commit_start = Instant::now();
        rw.commit().unwrap();
        reth_totals.commit += commit_start.elapsed();

        reth_totals.total += total_start.elapsed();
    }

    drop(factory);

    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();

    let mptdb_prepop = prepopulate_mptdb_in_chunks(&mut store, &addrs, 30, PREPOP_CHUNK_SIZE);
    println!("  frontier after prepop: {:?}", store.frontier());

    let mut mptdb_totals = MptdbTotals::default();
    for block in &blocks {
        let apply_start = Instant::now();
        store.apply_bundle_state(block).unwrap();
        mptdb_totals.apply += apply_start.elapsed();

        let ((_version, _root), profile): ((i64, B256), CommitProfile) =
            store.commit_with_profile().unwrap();
        mptdb_totals.trie_load += profile.apply_get_or_load_storage_tries;
        mptdb_totals.slot_updates += profile.apply_storage_slot_updates;
        mptdb_totals.l3_latest += profile.apply_l3_latest_load;
        mptdb_totals.l3_published += profile.apply_l3_published_load;
        mptdb_totals.to_tree += profile.apply_l3_into_tree;
        mptdb_totals.commit += profile.total_commit;
        mptdb_totals.storage_roots += profile.storage_roots;
        mptdb_totals.account_updates += profile.account_updates;
        mptdb_totals.account_root += profile.account_root_and_blobs;
        mptdb_totals.wal_append += profile.wal_append;
        mptdb_totals.wal_replay += profile.wal_replay;
        mptdb_totals.durable_materialize += profile.durable_materialize;
        mptdb_totals.published_materialize += profile.published_materialize;
        mptdb_totals.persist += profile.persist_and_manifest;
        mptdb_totals.persist_batch += profile.persist_batch;
        mptdb_totals.manifest_save += profile.manifest_save;
        mptdb_totals.publish_generation += profile.publish_generation;
        mptdb_totals.open_published_store += profile.open_published_store;
        mptdb_totals.cache_publish += profile.cache_publish;
        mptdb_totals.storage_segment_build += profile.storage_segment_build;
        mptdb_totals.storage_root_hashing += profile.storage_root_hashing;
        mptdb_totals.durable_version_lag += profile.durable_version_lag;
        mptdb_totals.published_version_lag += profile.published_version_lag;
        mptdb_totals.l2_hits += profile.apply_l2_hits;
        mptdb_totals.l3_hits += profile.apply_l3_latest_hits +
            profile.apply_l3_published_hits +
            profile.apply_l3_published_post_flush_hits;
    }
    store.close().unwrap();

    let blocks_len = blocks.len() as u32;

    println!("\nreth");
    println!("  pre-pop total:       {} ms", fmt_ms(reth_prepop));
    println!("  per-block total:     {} ms", fmt_ms(reth_totals.total / blocks_len));
    println!("  hash+sort:           {} ms", fmt_ms(reth_totals.hash_and_sort / blocks_len));
    println!("  root_with_updates:   {} ms", fmt_ms(reth_totals.root_updates / blocks_len));
    println!("  write_hashed_state:  {} ms", fmt_ms(reth_totals.write_hashed / blocks_len));
    println!("  write_trie_updates:  {} ms", fmt_ms(reth_totals.write_trie / blocks_len));
    println!("  commit:              {} ms", fmt_ms(reth_totals.commit / blocks_len));

    println!("\nmpt-db");
    println!("  pre-pop total:       {} ms", fmt_ms(mptdb_prepop));
    println!(
        "  per-block total:     {} ms",
        fmt_ms((mptdb_totals.apply + mptdb_totals.commit) / blocks_len)
    );
    println!("  apply_bundle_state:  {} ms", fmt_ms(mptdb_totals.apply / blocks_len));
    println!("  trie_load:           {} ms", fmt_ms(mptdb_totals.trie_load / blocks_len));
    println!("  slot_updates:        {} ms", fmt_ms(mptdb_totals.slot_updates / blocks_len));
    println!("  commit:              {} ms", fmt_ms(mptdb_totals.commit / blocks_len));
    println!("  storage_roots:       {} ms", fmt_ms(mptdb_totals.storage_roots / blocks_len));
    println!("  account_updates:     {} ms", fmt_ms(mptdb_totals.account_updates / blocks_len));
    println!("  account_root:        {} ms", fmt_ms(mptdb_totals.account_root / blocks_len));
    println!("  wal_append:          {} ms", fmt_ms(mptdb_totals.wal_append / blocks_len));
    println!(
        "  segment_build:       {} ms",
        fmt_ms(mptdb_totals.storage_segment_build / blocks_len)
    );
    println!(
        "  root_hashing:        {} ms",
        fmt_ms(mptdb_totals.storage_root_hashing / blocks_len)
    );
    println!(
        "  avg hits/block:      L2={}, L3={}",
        mptdb_totals.l2_hits / blocks.len() as u64,
        mptdb_totals.l3_hits / blocks.len() as u64
    );

    let reth_avg = reth_totals.total / blocks_len;
    let mptdb_avg = (mptdb_totals.apply + mptdb_totals.commit) / blocks_len;
    println!(
        "\napprox ratio (single run): mpt-db / reth = {:.2}x",
        mptdb_avg.as_secs_f64() / reth_avg.as_secs_f64()
    );
}
