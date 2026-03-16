//! Correctness tests: mpt-db MPT vs reth MPT+MDBX.
//!
//! For identical BundleState inputs, both engines must produce the same state root.
//! reth's MPT+MDBX result is the ground truth.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use reth_provider::{test_utils::create_test_provider_factory, StateWriter, TrieWriter};
use reth_trie::{HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    slots_per: usize,
    block_idx: usize,
    count: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for i in 0..count {
        let idx = rng.random_range(0..addresses.len());
        let addr = addresses[idx];
        let nonce = (block_idx * count + i) as u64;
        let balance = U256::from(2_000_000 * (block_idx * count + i + 1));

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

/// Compute state root using reth's full MPT+MDBX pipeline.
/// Applies pre_pop first, then processes each block bundle incrementally.
fn reth_roots(
    pre_pop: &revm_database::BundleState,
    block_bundles: &[revm_database::BundleState],
) -> Vec<B256> {
    let factory = create_test_provider_factory();

    // Apply pre-pop
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(pre_pop.state());
    let sorted = hashed.into_sorted();
    let rw = factory.provider_rw().unwrap();
    rw.write_hashed_state(&sorted).unwrap();
    let (_, updates) = StateRoot::from_tx(rw.tx_ref()).root_with_updates().unwrap();
    rw.write_trie_updates(updates).unwrap();
    rw.commit().unwrap();

    // Process blocks
    let mut roots = Vec::with_capacity(block_bundles.len());
    for bundle in block_bundles {
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let sorted = hashed.into_sorted();
        let rw = factory.provider_rw().unwrap();
        let (root, updates) = StateRoot::overlay_root_with_updates(rw.tx_ref(), &sorted).unwrap();
        rw.write_hashed_state(&sorted).unwrap();
        rw.write_trie_updates(updates).unwrap();
        rw.commit().unwrap();
        roots.push(root);
    }
    roots
}

/// Compute state root using mpt-db's MptCommitStore pipeline.
fn mptdb_roots(
    pre_pop: &revm_database::BundleState,
    block_bundles: &[revm_database::BundleState],
) -> Vec<B256> {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    // Apply pre-pop
    store.apply_bundle_state(pre_pop).unwrap();
    store.commit().unwrap();

    // Process blocks
    let mut roots = Vec::with_capacity(block_bundles.len());
    for bundle in block_bundles {
        store.apply_bundle_state(bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        roots.push(root);
    }
    store.close().unwrap();
    roots
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fresh state: 100 accounts with 5 slots each → single block root must match.
#[test]
fn correctness_fresh_100_accounts_5_slots() {
    let mut rng = StdRng::seed_from_u64(100);
    let (bundle, _) = generate_accounts(100, 5, &mut rng);

    let reth = reth_roots(&revm_database::BundleState::default(), &[bundle.clone()]);
    let mptdb = mptdb_roots(&revm_database::BundleState::default(), &[bundle]);

    assert_eq!(reth.len(), 1);
    assert_eq!(mptdb.len(), 1);
    assert_eq!(
        reth[0], mptdb[0],
        "fresh 100 accounts: reth root {:?} != mptdb root {:?}",
        reth[0], mptdb[0]
    );
}

/// Pre-populated 500 accounts → 3 blocks of 100 updates each → every block root must match.
#[test]
fn correctness_prepop_500_3_blocks() {
    let mut rng = StdRng::seed_from_u64(200);
    let (pre_pop, addresses) = generate_accounts(500, 3, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(201);
    let blocks: Vec<_> =
        (0..3).map(|i| generate_updates(&addresses, 3, i, 100, &mut rng_blocks)).collect();

    let reth = reth_roots(&pre_pop, &blocks);
    let mptdb = mptdb_roots(&pre_pop, &blocks);

    assert_eq!(reth.len(), 3);
    assert_eq!(mptdb.len(), 3);
    for (i, (r, m)) in reth.iter().zip(mptdb.iter()).enumerate() {
        assert_eq!(r, m, "block {} root mismatch: reth={:?} mptdb={:?}", i + 1, r, m);
    }
}

/// Account-only (no storage) → root must match.
#[test]
fn correctness_account_only_no_storage() {
    let mut rng = StdRng::seed_from_u64(300);
    let (bundle, _) = generate_accounts(200, 0, &mut rng);

    let reth = reth_roots(&revm_database::BundleState::default(), &[bundle.clone()]);
    let mptdb = mptdb_roots(&revm_database::BundleState::default(), &[bundle]);

    assert_eq!(reth[0], mptdb[0], "account-only: roots must match");
}

/// Storage-heavy: 50 accounts with 50 slots each → root must match.
#[test]
fn correctness_storage_heavy() {
    let mut rng = StdRng::seed_from_u64(400);
    let (bundle, _) = generate_accounts(50, 50, &mut rng);

    let reth = reth_roots(&revm_database::BundleState::default(), &[bundle.clone()]);
    let mptdb = mptdb_roots(&revm_database::BundleState::default(), &[bundle]);

    assert_eq!(reth[0], mptdb[0], "storage-heavy: roots must match");
}

/// 10-block incremental: 1K pre-pop, 200 updates/block → all roots must match.
#[test]
fn correctness_10_blocks_incremental() {
    let mut rng = StdRng::seed_from_u64(500);
    let (pre_pop, addresses) = generate_accounts(1000, 5, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(501);
    let blocks: Vec<_> =
        (0..10).map(|i| generate_updates(&addresses, 5, i, 200, &mut rng_blocks)).collect();

    let reth = reth_roots(&pre_pop, &blocks);
    let mptdb = mptdb_roots(&pre_pop, &blocks);

    assert_eq!(reth.len(), 10);
    assert_eq!(mptdb.len(), 10);
    for (i, (r, m)) in reth.iter().zip(mptdb.iter()).enumerate() {
        assert_eq!(r, m, "block {} root mismatch: reth={:?} mptdb={:?}", i + 1, r, m);
    }
}

/// Mixed workload: account creates + updates + deletes (slot=0) → root must match.
#[test]
fn correctness_mixed_with_deletes() {
    let mut rng = StdRng::seed_from_u64(600);
    let (pre_pop, addresses) = generate_accounts(200, 3, &mut rng);

    // Block 1: update some accounts, zero-out some slots (delete)
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for (i, addr) in addresses.iter().take(50).enumerate() {
        let info = AccountInfo {
            nonce: (i + 100) as u64,
            balance: U256::from(5_000_000u64),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        };

        let mut storage = StorageWithOriginalValues::default();
        // Delete slot 0 (set to zero)
        let mut slot_bytes = [0u8; 32];
        slot_bytes[24..32].copy_from_slice(&0u64.to_be_bytes());
        storage.insert(
            B256::from(slot_bytes).into(),
            StorageSlot::new_changed(U256::from(1u64), U256::ZERO),
        );
        // Update slot 1
        slot_bytes[24..32].copy_from_slice(&1u64.to_be_bytes());
        storage.insert(
            B256::from(slot_bytes).into(),
            StorageSlot::new_changed(U256::from(2u64), U256::from(9999u64)),
        );

        state.insert(
            *addr,
            revm_database::BundleAccount {
                info: Some(info),
                original_info: None,
                status: AccountStatus::Changed,
                storage,
            },
        );
    }

    let block1 = revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    };

    let reth = reth_roots(&pre_pop, &[block1.clone()]);
    let mptdb = mptdb_roots(&pre_pop, &[block1]);

    assert_eq!(reth[0], mptdb[0], "mixed with deletes: roots must match");
}

/// Self-destruct + recreate in same block → root must match.
#[test]
fn correctness_selfdestruct_recreate() {
    let mut rng = StdRng::seed_from_u64(700);
    let (pre_pop, addresses) = generate_accounts(100, 5, &mut rng);

    // Block 1: destroy first 10 accounts and recreate them with new state
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for (i, addr) in addresses.iter().take(10).enumerate() {
        let new_info = AccountInfo {
            nonce: 0,
            balance: U256::from(777u64),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        };

        let mut storage = StorageWithOriginalValues::default();
        // New single slot after recreate
        let mut slot_bytes = [0u8; 32];
        slot_bytes[24..32].copy_from_slice(&(100 + i as u64).to_be_bytes());
        storage.insert(
            B256::from(slot_bytes).into(),
            StorageSlot::new_changed(U256::ZERO, U256::from(888u64)),
        );

        state.insert(
            *addr,
            revm_database::BundleAccount {
                info: Some(new_info),
                original_info: None,
                status: AccountStatus::DestroyedChanged,
                storage,
            },
        );
    }

    let block1 = revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    };

    let reth = reth_roots(&pre_pop, &[block1.clone()]);
    let mptdb = mptdb_roots(&pre_pop, &[block1]);

    assert_eq!(reth[0], mptdb[0], "selfdestruct+recreate: roots must match");
}

/// Close + reopen between every block → forces full RocksDB read-back.
/// Verifies that persisted data roundtrips correctly through the DB.
#[test]
fn correctness_reopen_between_blocks() {
    let mut rng = StdRng::seed_from_u64(800);
    let (pre_pop, addresses) = generate_accounts(200, 5, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(801);
    let blocks: Vec<_> =
        (0..5).map(|i| generate_updates(&addresses, 5, i, 50, &mut rng_blocks)).collect();

    // reth reference (continuous, no reopen needed)
    let reth = reth_roots(&pre_pop, &blocks);

    // mptdb with close+reopen between each block
    let dir = TempDir::new().unwrap();
    let mut mptdb_roots_vec = Vec::new();

    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&pre_pop).unwrap();
        store.commit().unwrap();
        store.close().unwrap();
    }

    for bundle in &blocks {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        mptdb_roots_vec.push(root);
        store.close().unwrap();
    }

    assert_eq!(reth.len(), mptdb_roots_vec.len());
    for (i, (r, m)) in reth.iter().zip(mptdb_roots_vec.iter()).enumerate() {
        assert_eq!(r, m, "block {} root mismatch after reopen: reth={:?} mptdb={:?}", i + 1, r, m);
    }
}
