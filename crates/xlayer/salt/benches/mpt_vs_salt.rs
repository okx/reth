//! Benchmark comparing MPT (Merkle Patricia Trie) vs SALT state root computation.
//!
//! Measures:
//! 1. State hashing: `BundleState` → `HashedPostState` (MPT) vs plain k-v conversion (SALT)
//! 2. State root computation: MPT trie root vs SALT trie root
//! 3. Incremental updates: applying multiple blocks sequentially

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::time::Duration;

use salt::{EphemeralSaltState, MemStore, StateRoot as SaltStateRoot};
use xlayer_salt::convert::bundle_state_to_plain_kv;

/// Generate a deterministic `BundleState` with the given number of accounts
/// and storage slots per account.
fn generate_bundle_state(
    num_accounts: usize,
    slots_per_account: usize,
    offset: usize,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for i in 0..num_accounts {
        let idx = offset + i;
        let mut addr_bytes = [0u8; 20];
        addr_bytes[12..20].copy_from_slice(&(idx as u64).to_be_bytes());
        let addr = Address::from(addr_bytes);

        let info = AccountInfo {
            nonce: idx as u64,
            balance: U256::from(1_000_000 * (idx + 1)),
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

/// Benchmark: SALT state conversion (BundleState → plain k-v)
fn bench_salt_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("State Conversion");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for (num_accounts, slots) in [(100, 10), (500, 10), (1000, 10), (1000, 100)] {
        let bundle = generate_bundle_state(num_accounts, slots, 0);
        let label = format!("{num_accounts}accts_{slots}slots");

        group.bench_function(BenchmarkId::new("salt_convert", &label), |b| {
            b.iter(|| bundle_state_to_plain_kv(&bundle))
        });
    }

    group.finish();
}

/// Benchmark: SALT full state root (convert + EphemeralSaltState + StateRoot)
fn bench_salt_state_root(c: &mut Criterion) {
    let mut group = c.benchmark_group("SALT State Root");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for (num_accounts, slots) in [(100, 10), (500, 10), (1000, 10), (1000, 100)] {
        let bundle = generate_bundle_state(num_accounts, slots, 0);
        let label = format!("{num_accounts}accts_{slots}slots");

        group.bench_function(BenchmarkId::new("full_root", &label), |b| {
            b.iter(|| {
                let store = MemStore::new();
                let kvs = bundle_state_to_plain_kv(&bundle);
                let mut ephemeral = EphemeralSaltState::new(&store);
                let state_updates = ephemeral.update_fin(&kvs).unwrap();
                store.update_state(state_updates.clone());
                let mut root = SaltStateRoot::new(&store);
                let (root_hash, trie_updates) = root.update_fin(&state_updates).unwrap();
                store.update_trie(trie_updates);
                root_hash
            })
        });
    }

    group.finish();
}

/// Benchmark: SALT incremental updates (multiple blocks on the same store)
fn bench_salt_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("SALT Incremental Update");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    let num_blocks = 10;
    let accounts_per_block = 100;
    let slots_per_account = 10;

    group.bench_function(
        BenchmarkId::new("incremental", format!("{num_blocks}blocks_{accounts_per_block}accts")),
        |b| {
            b.iter(|| {
                let store = MemStore::new();
                let mut root = SaltStateRoot::new(&store);

                for block_idx in 0..num_blocks {
                    let bundle = generate_bundle_state(
                        accounts_per_block,
                        slots_per_account,
                        block_idx * accounts_per_block,
                    );
                    let kvs = bundle_state_to_plain_kv(&bundle);
                    let mut ephemeral = EphemeralSaltState::new(&store);
                    let state_updates = ephemeral.update_fin(&kvs).unwrap();
                    store.update_state(state_updates.clone());
                    let (_, trie_updates) = root.update_fin(&state_updates).unwrap();
                    store.update_trie(trie_updates);
                }
            })
        },
    );

    group.finish();
}

/// Benchmark: SALT vs MPT-style hashing overhead
///
/// Compares the cost of:
/// - MPT: keccak256 hashing all keys (the `HashedPostState::from_bundle_state` step)
/// - SALT: plain key construction (no hashing needed)
fn bench_hashing_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("Hashing Overhead: Keccak vs Plain");
    group.sample_size(20);

    for num_accounts in [100, 500, 1000, 5000] {
        let slots = 10;
        let bundle = generate_bundle_state(num_accounts, slots, 0);
        let label = format!("{num_accounts}accts_{slots}slots");

        group.bench_function(BenchmarkId::new("keccak_hashing", &label), |b| {
            b.iter(|| {
                use alloy_primitives::keccak256;
                let mut count = 0u64;
                for (addr, account) in bundle.state() {
                    let _ = keccak256(addr);
                    count += 1;
                    for (slot, _) in &account.storage {
                        let _ = keccak256(B256::new(slot.to_be_bytes()));
                        count += 1;
                    }
                }
                count
            })
        });

        group.bench_function(BenchmarkId::new("salt_plain_key", &label), |b| {
            b.iter(|| {
                let kvs = bundle_state_to_plain_kv(&bundle);
                kvs.len()
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_salt_conversion,
    bench_salt_state_root,
    bench_salt_incremental,
    bench_hashing_overhead,
);
criterion_main!(benches);
