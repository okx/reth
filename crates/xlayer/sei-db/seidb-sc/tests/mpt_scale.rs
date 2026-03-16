//! Large-scale validation tests for MptCommitStore.
//!
//! These tests are `#[ignore]`d so they never run in CI.
//! Run them manually with:
//!
//! ```bash
//! cargo test -p seidb-sc --test mpt_scale -- --ignored --nocapture
//! ```

use alloy_primitives::{Address, U256};
use alloy_trie::KECCAK_EMPTY;
use revm_database::{states::StorageSlot, AccountStatus, BundleAccount, BundleState};
use revm_state::AccountInfo;
use seidb_sc::mpt::{MptCommitStore, MptCommitter};
use std::time::Instant;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deterministic address from a u64 index.
fn make_address(i: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(bytes)
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

fn make_bundle(
    accounts: Vec<(Address, Option<AccountInfo>, AccountStatus, Vec<(U256, U256, U256)>)>,
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

// ---------------------------------------------------------------------------
// S2.1 -- 100K accounts, no storage
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_1_100k_accounts_no_storage() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    const N: u64 = 100_000;
    let t0 = Instant::now();

    let accounts: Vec<_> = (0..N)
        .map(|i| (make_address(i), Some(default_info(i, i * 10)), AccountStatus::Changed, vec![]))
        .collect();

    let bundle = make_bundle(accounts);
    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();

    let elapsed_commit = t0.elapsed();
    assert_eq!(ver, 1);
    assert_ne!(root, alloy_trie::EMPTY_ROOT_HASH, "root must not be empty");

    // Close and reopen to verify persistence
    store.close().unwrap();
    let mut store2 = MptCommitStore::open(dir.path(), false).unwrap();
    assert_eq!(store2.version(), 1);

    // Commit empty block -- root must be unchanged
    store2.apply_bundle_state(&BundleState::default()).unwrap();
    let (ver2, root2) = store2.commit().unwrap();
    let reopen_consistent = root2 == root;

    eprintln!(
        "S2.1: accounts={N}, version={ver}, root={root:?}, \
         reopen_version={ver2}, reopen_consistent={reopen_consistent}, \
         commit_time={elapsed_commit:?}"
    );

    assert!(reopen_consistent, "root mismatch after reopen");
}

// ---------------------------------------------------------------------------
// S2.2 -- 100K accounts, 4 storage slots each
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_2_100k_accounts_4_slots_each() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    const N: u64 = 100_000;
    let t0 = Instant::now();

    let accounts: Vec<_> = (0..N)
        .map(|i| {
            let slots: Vec<(U256, U256, U256)> = (0..4)
                .map(|s| {
                    let key = U256::from(i * 4 + s);
                    let val = U256::from(i * 100 + s + 1);
                    (key, U256::ZERO, val)
                })
                .collect();
            (make_address(i), Some(default_info(i, i * 10)), AccountStatus::Changed, slots)
        })
        .collect();

    let bundle = make_bundle(accounts);
    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();

    let elapsed_commit = t0.elapsed();
    assert_eq!(ver, 1);
    assert_ne!(root, alloy_trie::EMPTY_ROOT_HASH);

    // Close and reopen
    store.close().unwrap();
    let mut store2 = MptCommitStore::open(dir.path(), false).unwrap();
    assert_eq!(store2.version(), 1);

    store2.apply_bundle_state(&BundleState::default()).unwrap();
    let (ver2, root2) = store2.commit().unwrap();
    let reopen_consistent = root2 == root;

    eprintln!(
        "S2.2: accounts={N}, slots_per_account=4, version={ver}, root={root:?}, \
         reopen_version={ver2}, reopen_consistent={reopen_consistent}, \
         commit_time={elapsed_commit:?}"
    );

    assert!(reopen_consistent, "root mismatch after reopen");
}

// ---------------------------------------------------------------------------
// S2.3 -- 1M accounts, no storage (batched in chunks of 10K)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_3_1m_accounts_no_storage() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    const TOTAL: u64 = 1_000_000;
    const CHUNK: u64 = 10_000;
    let t0 = Instant::now();

    let mut last_root = alloy_trie::EMPTY_ROOT_HASH;
    let mut last_ver: i64 = 0;

    for chunk_start in (0..TOTAL).step_by(CHUNK as usize) {
        let chunk_end = (chunk_start + CHUNK).min(TOTAL);
        let accounts: Vec<_> = (chunk_start..chunk_end)
            .map(|i| {
                (make_address(i), Some(default_info(i, i * 10)), AccountStatus::Changed, vec![])
            })
            .collect();

        let bundle = make_bundle(accounts);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, root) = store.commit().unwrap();
        last_ver = ver;
        last_root = root;
    }

    let elapsed_total = t0.elapsed();
    let num_commits = TOTAL / CHUNK;
    assert_eq!(last_ver, num_commits as i64);
    assert_ne!(last_root, alloy_trie::EMPTY_ROOT_HASH);

    // Close and reopen
    store.close().unwrap();
    let mut store2 = MptCommitStore::open(dir.path(), false).unwrap();
    assert_eq!(store2.version(), last_ver);

    store2.apply_bundle_state(&BundleState::default()).unwrap();
    let (_, root2) = store2.commit().unwrap();
    let reopen_consistent = root2 == last_root;

    eprintln!(
        "S2.3: accounts={TOTAL}, chunks={num_commits}, version={last_ver}, \
         root={last_root:?}, reopen_consistent={reopen_consistent}, \
         total_time={elapsed_total:?}"
    );

    assert!(reopen_consistent, "root mismatch after reopen");
}

// ---------------------------------------------------------------------------
// S2.4 -- 1M accounts, 10% have 2 storage slots (batched)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_4_1m_accounts_sparse_storage() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    const TOTAL: u64 = 1_000_000;
    const CHUNK: u64 = 10_000;
    let t0 = Instant::now();

    let mut last_root = alloy_trie::EMPTY_ROOT_HASH;
    let mut last_ver: i64 = 0;
    let mut total_with_storage: u64 = 0;

    for chunk_start in (0..TOTAL).step_by(CHUNK as usize) {
        let chunk_end = (chunk_start + CHUNK).min(TOTAL);
        let accounts: Vec<_> = (chunk_start..chunk_end)
            .map(|i| {
                // 10% of accounts get 2 storage slots
                let slots = if i % 10 == 0 {
                    total_with_storage += 1;
                    vec![
                        (U256::from(i * 2), U256::ZERO, U256::from(i + 1)),
                        (U256::from(i * 2 + 1), U256::ZERO, U256::from(i + 2)),
                    ]
                } else {
                    vec![]
                };
                (make_address(i), Some(default_info(i, i * 10)), AccountStatus::Changed, slots)
            })
            .collect();

        let bundle = make_bundle(accounts);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, root) = store.commit().unwrap();
        last_ver = ver;
        last_root = root;
    }

    let elapsed_total = t0.elapsed();
    assert_ne!(last_root, alloy_trie::EMPTY_ROOT_HASH);

    // Close and reopen
    store.close().unwrap();
    let mut store2 = MptCommitStore::open(dir.path(), false).unwrap();
    assert_eq!(store2.version(), last_ver);

    store2.apply_bundle_state(&BundleState::default()).unwrap();
    let (_, root2) = store2.commit().unwrap();
    let reopen_consistent = root2 == last_root;

    eprintln!(
        "S2.4: accounts={TOTAL}, with_storage={total_with_storage}, \
         version={last_ver}, root={last_root:?}, \
         reopen_consistent={reopen_consistent}, total_time={elapsed_total:?}"
    );

    assert!(reopen_consistent, "root mismatch after reopen");
}

// ---------------------------------------------------------------------------
// S2.5 -- 10 incremental blocks (10K new + 1K updates each)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn s2_5_10_block_incremental_large() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    const BLOCKS: u64 = 10;
    const NEW_PER_BLOCK: u64 = 10_000;
    const UPDATE_PER_BLOCK: u64 = 1_000;

    let t0 = Instant::now();
    let mut prev_root = alloy_trie::EMPTY_ROOT_HASH;
    let mut versions: Vec<i64> = Vec::new();
    let mut roots: Vec<alloy_primitives::B256> = Vec::new();

    for block in 0..BLOCKS {
        let base = block * NEW_PER_BLOCK;

        // New accounts for this block
        let mut accounts: Vec<_> = (base..base + NEW_PER_BLOCK)
            .map(|i| {
                (make_address(i), Some(default_info(i, i * 10)), AccountStatus::Changed, vec![])
            })
            .collect();

        // Update existing accounts from earlier blocks (if any exist)
        if block > 0 {
            let update_base = (block - 1) * NEW_PER_BLOCK;
            for j in 0..UPDATE_PER_BLOCK {
                let i = update_base + j;
                accounts.push((
                    make_address(i),
                    Some(default_info(i + 1000, (i + 1000) * 10)),
                    AccountStatus::Changed,
                    vec![],
                ));
            }
        }

        let bundle = make_bundle(accounts);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, root) = store.commit().unwrap();

        // Version must be monotonically increasing
        assert_eq!(ver, (block + 1) as i64, "version not monotonic at block {block}");
        // Root must change each block (new accounts are always added)
        assert_ne!(root, prev_root, "root unchanged at block {block}");

        versions.push(ver);
        roots.push(root);
        prev_root = root;
    }

    let elapsed_total = t0.elapsed();

    let final_ver = *versions.last().unwrap();
    let final_root = *roots.last().unwrap();

    // Close and reopen
    store.close().unwrap();
    let mut store2 = MptCommitStore::open(dir.path(), false).unwrap();
    let reopen_ver = store2.version();

    store2.apply_bundle_state(&BundleState::default()).unwrap();
    let (_, reopen_root) = store2.commit().unwrap();
    let reopen_consistent = reopen_root == final_root;

    eprintln!(
        "S2.5: blocks={BLOCKS}, new_per_block={NEW_PER_BLOCK}, \
         update_per_block={UPDATE_PER_BLOCK}, \
         final_version={final_ver}, final_root={final_root:?}, \
         reopen_version={reopen_ver}, reopen_consistent={reopen_consistent}, \
         total_time={elapsed_total:?}"
    );

    assert_eq!(reopen_ver, final_ver, "version mismatch after reopen");
    assert!(reopen_consistent, "root mismatch after reopen");
}
