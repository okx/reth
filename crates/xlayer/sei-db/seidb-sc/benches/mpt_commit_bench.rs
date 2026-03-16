use alloy_primitives::{Address, B256, U256};
use alloy_trie::KECCAK_EMPTY;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
use seidb_sc::mpt::{MptCommitStore, MptCommitter};
use std::time::Instant;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_bundle(
    accounts: Vec<(
        Address,
        Option<AccountInfo>,
        revm_database::AccountStatus,
        Vec<(U256, U256, U256)>,
    )>,
) -> BundleState {
    let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
        alloy_primitives::map::HashMap::default();
    for (address, info, status, storage) in accounts {
        let storage_map: revm_database::StorageWithOriginalValues = storage
            .into_iter()
            .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
            .collect();
        let bundle_account = BundleAccount::new(None, info, storage_map, status);
        state.insert(address, bundle_account);
    }
    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

fn default_info(nonce: u64, balance: u64) -> AccountInfo {
    AccountInfo {
        nonce,
        balance: U256::from(balance),
        code_hash: KECCAK_EMPTY,
        account_id: None,
        code: None,
    }
}

/// Deterministic address from a seed index.
fn addr(i: usize) -> Address {
    Address::from_word(B256::from(U256::from(i + 1)))
}

// ---------------------------------------------------------------------------
// B3.1  Account-heavy (no storage)
// ---------------------------------------------------------------------------

fn account_heavy_bundle(n: usize) -> BundleState {
    let accounts: Vec<_> = (0..n)
        .map(|i| {
            (
                addr(i),
                Some(default_info(i as u64, 1000 + i as u64)),
                revm_database::AccountStatus::Changed,
                vec![],
            )
        })
        .collect();
    make_bundle(accounts)
}

fn bench_account_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("B3.1_account_heavy");

    for &size in &[100, 1_000, 10_000] {
        let bundle = account_heavy_bundle(size);

        group.bench_with_input(BenchmarkId::new("apply_commit", size), &bundle, |b, bun| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut store = MptCommitStore::open(dir.path(), false).unwrap();
                store.apply_bundle_state(bun).unwrap();
                let (_ver, _root) = store.commit().unwrap();
                drop(store);
                drop(dir);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// B3.2  Storage-heavy
// ---------------------------------------------------------------------------

fn storage_heavy_bundle(num_accounts: usize, slots_per_account: usize) -> BundleState {
    let accounts: Vec<_> = (0..num_accounts)
        .map(|i| {
            let storage: Vec<(U256, U256, U256)> = (0..slots_per_account)
                .map(|s| (U256::from(s), U256::ZERO, U256::from(s + 1)))
                .collect();
            (
                addr(i),
                Some(default_info(1, 1000 + i as u64)),
                revm_database::AccountStatus::Changed,
                storage,
            )
        })
        .collect();
    make_bundle(accounts)
}

fn bench_storage_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("B3.2_storage_heavy");

    // (accounts, slots_per_account)
    let configs = [(100, 10), (100, 100), (1_000, 10)];

    for (accts, slots) in configs {
        let bundle = storage_heavy_bundle(accts, slots);
        let label = format!("{accts}acct_{slots}slot");

        group.bench_with_input(BenchmarkId::new("apply_commit", &label), &bundle, |b, bun| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let mut store = MptCommitStore::open(dir.path(), false).unwrap();
                store.apply_bundle_state(bun).unwrap();
                let (_ver, _root) = store.commit().unwrap();
                drop(store);
                drop(dir);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// B3.3  Mixed workload (accounts + storage + deletes + selfdestructs)
// ---------------------------------------------------------------------------

fn mixed_bundle() -> BundleState {
    let mut accounts = Vec::new();

    for i in 0..500usize {
        let address = addr(i);

        if i % 10 == 0 {
            // ~50 accounts: self-destructed (wipe)
            accounts.push((address, None, revm_database::AccountStatus::Destroyed, vec![]));
        } else if i % 2 == 0 {
            // ~225 accounts: with storage, some slots deleted (present = 0)
            let storage: Vec<(U256, U256, U256)> = (0..5)
                .map(|s| {
                    let present = if s % 3 == 0 { U256::ZERO } else { U256::from(s * 100 + i + 1) };
                    (U256::from(s), U256::from(s + 1), present)
                })
                .collect();
            accounts.push((
                address,
                Some(default_info(i as u64, 2000 + i as u64)),
                revm_database::AccountStatus::Changed,
                storage,
            ));
        } else {
            // ~225 accounts: account-only, no storage
            accounts.push((
                address,
                Some(default_info(i as u64, 1000 + i as u64)),
                revm_database::AccountStatus::Changed,
                vec![],
            ));
        }
    }

    make_bundle(accounts)
}

fn bench_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("B3.3_mixed");
    let bundle = mixed_bundle();

    group.bench_function("500acct_mixed_apply_commit", |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.apply_bundle_state(&bundle).unwrap();
            let (_ver, _root) = store.commit().unwrap();
            drop(store);
            drop(dir);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// B3.4  Multi-block incremental (reuse store across blocks)
// ---------------------------------------------------------------------------

/// Build a per-block bundle where each block mutates a distinct set of 100 accounts.
fn block_bundle(block_idx: usize) -> BundleState {
    let base = block_idx * 100;
    let accounts: Vec<_> = (0..100)
        .map(|i| {
            let global_i = base + i;
            let storage: Vec<(U256, U256, U256)> =
                (0..3).map(|s| (U256::from(s), U256::ZERO, U256::from(s + global_i + 1))).collect();
            (
                addr(global_i),
                Some(default_info(global_i as u64, 500 + global_i as u64)),
                revm_database::AccountStatus::Changed,
                storage,
            )
        })
        .collect();
    make_bundle(accounts)
}

fn bench_multi_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("B3.4_multi_block");

    for &num_blocks in &[10, 100] {
        // Pre-generate all block bundles so generation time is excluded.
        let bundles: Vec<BundleState> = (0..num_blocks).map(block_bundle).collect();

        group.bench_with_input(
            BenchmarkId::new("incremental", num_blocks),
            &bundles,
            |b, bundles| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let dir = TempDir::new().unwrap();
                        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

                        let start = Instant::now();
                        for bun in bundles {
                            store.apply_bundle_state(bun).unwrap();
                            let _ = store.commit().unwrap();
                        }
                        total += start.elapsed();

                        drop(store);
                        drop(dir);
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

criterion_group!(benches, bench_account_heavy, bench_storage_heavy, bench_mixed, bench_multi_block,);
criterion_main!(benches);
