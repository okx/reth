use alloy_primitives::{Address, B256, U256};
use alloy_trie::KECCAK_EMPTY;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
use seidb_sc::mpt::{MptCommitStore, MptCommitter};
use tempfile::TempDir;

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

fn storage_heavy_bundle(num_accounts: usize, slots_per_account: usize) -> BundleState {
    let mut accounts = Vec::new();
    for i in 0..num_accounts {
        let addr = Address::from_word(B256::from(U256::from(i + 1)));
        let info = default_info(1, 1000 + i as u64);
        let storage: Vec<(U256, U256, U256)> = (0..slots_per_account)
            .map(|s| (U256::from(s), U256::ZERO, U256::from(s + 1)))
            .collect();
        accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, storage));
    }
    make_bundle(accounts)
}

/// B5.1: serial vs parallel storage roots.
fn bench_storage_roots(c: &mut Criterion) {
    let bundle = storage_heavy_bundle(200, 20);

    c.bench_function("mpt_commit_200acct_20slot_default", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                let store = MptCommitStore::open(dir.path(), false).unwrap();
                (dir, store, bundle.clone())
            },
            |(dir, mut store, bun)| {
                store.apply_bundle_state(&bun).unwrap();
                let _ = store.commit().unwrap();
                drop(store);
                drop(dir);
            },
            BatchSize::PerIteration,
        );
    });
}

/// B5.2: serial vs parallel account root hash.
fn bench_account_root_hash(c: &mut Criterion) {
    // Many accounts, no storage -> focuses on account trie hash
    let mut accounts = Vec::new();
    for i in 0..500usize {
        let addr = Address::from_word(B256::from(U256::from(i + 1)));
        let info = default_info(i as u64, 1000 + i as u64);
        accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, vec![]));
    }
    let bundle = make_bundle(accounts);

    c.bench_function("mpt_commit_500acct_no_storage_default", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                let store = MptCommitStore::open(dir.path(), false).unwrap();
                (dir, store, bundle.clone())
            },
            |(dir, mut store, bun)| {
                store.apply_bundle_state(&bun).unwrap();
                let _ = store.commit().unwrap();
                drop(store);
                drop(dir);
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group!(benches, bench_storage_roots, bench_account_root_hash);
criterion_main!(benches);
