//! Lightweight one-shot profile for mpt-db vs reth on the shared B4.5 dataset.
//!
//! Run with:
//! `PROTOC=/Users/louisliuxiong/golang/bin/protoc cargo test -p xlayer-salt --release --test
//! profile_mptdb_vs_reth profile_b4_5_single_run_compare -- --ignored --nocapture`

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use mptdb_sc::mpt::{CommitProfile, MptCommitStore, MptCommitter};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use reth_provider::{test_utils::create_test_provider_factory, StateWriter, TrieWriter};
use reth_trie::{updates::TrieUpdates, HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn generate_accounts(
    num: usize,
    slots_per: usize,
    rng: &mut StdRng,
) -> (revm_database::BundleState, Vec<Address>) {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    let mut addresses = Vec::with_capacity(num);
    let mut addr_buf = [0u8; 20];

    for i in 0..num {
        rng.fill_bytes(&mut addr_buf);
        let addr = Address::from(addr_buf);
        addresses.push(addr);

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

    let bundle = revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    };
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
    persist: Duration,
    l2_hits: u64,
    l3_hits: u64,
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

#[test]
#[ignore]
fn profile_b4_5_single_run_compare() {
    let mut rng = StdRng::seed_from_u64(4500);
    let (pre_pop, addrs) = generate_accounts(1_000_000, 10, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(4501);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addrs, 5_000, 10, i, &mut rng_blocks)).collect();

    println!("\n=== B4.5 Single-Run Compare ===");
    println!("Dataset: 1M pre-pop x 10 slots, 10 blocks x 5K updates");

    let factory = create_test_provider_factory();

    let t0 = Instant::now();
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(pre_pop.state());
    let sorted = hashed.into_sorted();
    let rw = factory.provider_rw().unwrap();
    rw.write_hashed_state(&sorted).unwrap();
    let (_, updates): (B256, TrieUpdates) =
        StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
    rw.write_trie_updates(updates).unwrap();
    rw.commit().unwrap();
    let reth_prepop = t0.elapsed();

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

    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let t0 = Instant::now();
    store.apply_bundle_state(&pre_pop).unwrap();
    let ((_version, _root), _profile) = store.commit_with_profile().unwrap();
    let mptdb_prepop = t0.elapsed();

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
        mptdb_totals.persist += profile.persist_and_manifest;
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
    println!("  persist:             {} ms", fmt_ms(mptdb_totals.persist / blocks_len));
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
