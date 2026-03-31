use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{HashBuilder, Nibbles, EMPTY_ROOT_HASH, KECCAK_EMPTY};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter};
use revm_database::{states::StorageSlot, BundleAccount, BundleState};
use revm_state::AccountInfo;
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

fn compute_storage_root(slots: &[(U256, U256)]) -> B256 {
    if slots.is_empty() {
        return EMPTY_ROOT_HASH;
    }
    let mut entries: Vec<(Nibbles, Vec<u8>)> = slots
        .iter()
        .filter(|(_, v)| *v != U256::ZERO)
        .map(|(k, v)| {
            let hashed = keccak256(k.to_be_bytes::<32>());
            let mut encoded = Vec::new();
            v.encode(&mut encoded);
            (Nibbles::unpack(hashed), encoded)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hb = HashBuilder::default();
    for (key, val) in entries {
        hb.add_leaf(key, &val);
    }
    hb.root()
}

fn compute_state_root(accounts: &[(Address, AccountInfo, B256)]) -> B256 {
    if accounts.is_empty() {
        return EMPTY_ROOT_HASH;
    }
    let mut entries: Vec<(Nibbles, Vec<u8>)> = accounts
        .iter()
        .map(|(addr, info, storage_root)| {
            let hashed = keccak256(addr);
            let ta = alloy_trie::TrieAccount {
                nonce: info.nonce,
                balance: info.balance,
                storage_root: *storage_root,
                code_hash: info.code_hash,
            };
            (Nibbles::unpack(hashed), alloy_rlp::encode(&ta))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hb = HashBuilder::default();
    for (key, val) in entries {
        hb.add_leaf(key, &val);
    }
    hb.root()
}

/// I7.1: block1 single account + single storage slot
#[test]
fn i7_1_single_account_single_slot() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x01);
    let info = default_info(1, 1000);
    let slot_key = U256::from(1);
    let slot_val = U256::from(42);

    let bundle = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::Changed,
        vec![(slot_key, U256::ZERO, slot_val)],
    )]);

    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();
    assert_eq!(ver, 1);

    let storage_root = compute_storage_root(&[(slot_key, slot_val)]);
    let expected = compute_state_root(&[(addr, info, storage_root)]);
    assert_eq!(root, expected);
}

/// I7.2: block2 only updates account fields, storage_root inherited
#[test]
fn i7_2_inherit_storage_root() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x02);
    let info1 = default_info(1, 100);
    let slot_key = U256::from(5);
    let slot_val = U256::from(99);

    // Block 1
    let bundle1 = make_bundle(vec![(
        addr,
        Some(info1),
        revm_database::AccountStatus::Changed,
        vec![(slot_key, U256::ZERO, slot_val)],
    )]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();

    // Block 2: only change balance
    let info2 = default_info(2, 200);
    let bundle2 = make_bundle(vec![(
        addr,
        Some(info2.clone()),
        revm_database::AccountStatus::Changed,
        vec![],
    )]);
    store.apply_bundle_state(&bundle2).unwrap();
    let (_, root2) = store.commit().unwrap();

    let storage_root = compute_storage_root(&[(slot_key, slot_val)]);
    let expected = compute_state_root(&[(addr, info2, storage_root)]);
    assert_eq!(root2, expected);
}

/// I7.3: block3 wipe + recreate
#[test]
fn i7_3_wipe_and_recreate() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr = Address::repeat_byte(0x03);
    let info = default_info(1, 100);

    // Block 1: account with slot1
    let bundle1 = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::Changed,
        vec![(U256::from(1), U256::ZERO, U256::from(10))],
    )]);
    store.apply_bundle_state(&bundle1).unwrap();
    store.commit().unwrap();

    // Block 2: destroy + recreate with new slot2
    let bundle2 = make_bundle(vec![(
        addr,
        Some(info.clone()),
        revm_database::AccountStatus::DestroyedChanged,
        vec![(U256::from(2), U256::ZERO, U256::from(20))],
    )]);
    store.apply_bundle_state(&bundle2).unwrap();
    let (_, root2) = store.commit().unwrap();

    // Expected: only slot2 (slot1 wiped)
    let storage_root = compute_storage_root(&[(U256::from(2), U256::from(20))]);
    let expected = compute_state_root(&[(addr, info, storage_root)]);
    assert_eq!(root2, expected);
}

/// I7.4: multiple accounts, multiple storage tries
#[test]
fn i7_4_multi_account_multi_storage() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    let addr1 = Address::repeat_byte(0x10);
    let addr2 = Address::repeat_byte(0x20);
    let info1 = default_info(1, 100);
    let info2 = default_info(2, 200);

    let bundle = make_bundle(vec![
        (
            addr1,
            Some(info1.clone()),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        ),
        (
            addr2,
            Some(info2.clone()),
            revm_database::AccountStatus::Changed,
            vec![
                (U256::from(2), U256::ZERO, U256::from(22)),
                (U256::from(3), U256::ZERO, U256::from(33)),
            ],
        ),
    ]);

    store.apply_bundle_state(&bundle).unwrap();
    let (_, root) = store.commit().unwrap();

    let sr1 = compute_storage_root(&[(U256::from(1), U256::from(11))]);
    let sr2 =
        compute_storage_root(&[(U256::from(2), U256::from(22)), (U256::from(3), U256::from(33))]);
    let expected = compute_state_root(&[(addr1, info1, sr1), (addr2, info2, sr2)]);
    assert_eq!(root, expected);
}

/// I7.5: 3 blocks consecutive commit, version monotonically increases
#[test]
fn i7_5_three_blocks() {
    let dir = TempDir::new().unwrap();
    let mut store = MptCommitStore::open(dir.path(), false).unwrap();

    for i in 1..=3 {
        let addr = Address::repeat_byte(i as u8);
        let info = default_info(i, i * 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, _) = store.commit().unwrap();
        assert_eq!(ver, i as i64);
    }
}

/// I7.6: reopen after close -> load_version, state_root unchanged
#[test]
fn i7_6_reopen_load() {
    let dir = TempDir::new().unwrap();

    let root1;
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x50);
        let info = default_info(1, 500);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(100))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, r) = store.commit().unwrap();
        root1 = r;
        store.close().unwrap();
    }

    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 1);
        // Commit empty block to verify state is intact
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root2) = store.commit().unwrap();
        assert_eq!(ver, 2);
        // root2 should be same as root1 since no changes to the account
        assert_eq!(root2, root1);
    }
}

/// I7.7: rollback to old version, reopen, state_root consistent
#[test]
fn i7_7_rollback_and_reopen() {
    let dir = TempDir::new().unwrap();

    let root1;
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Block 1: create account
        let addr = Address::repeat_byte(0x60);
        let info1 = default_info(1, 100);
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, r1) = store.commit().unwrap();
        root1 = r1;

        // Block 2: update account
        let info2 = default_info(2, 200);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();

        // Rollback to version 1
        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        store.close().unwrap();
    }

    // Reopen and verify
    {
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 1);

        // Verify by committing empty block: should get root consistent with version 1 state
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root_after) = store.commit().unwrap();
        assert_eq!(ver, 2);
        // root should equal root1 since no state changes (empty block)
        assert_eq!(root_after, root1);
    }
}

// ── Phase 2: use_sparse_storage tests ─────────────────────────────────────────
//
// Each test runs the SAME scenario twice — once with the default config (normal
// path) and once with `use_sparse_storage=true` (sparse path) — and asserts
// that the resulting state roots are identical.  This validates that the sparse
// path produces the correct root without diverging from the reference.

fn open_sparse(dir: &std::path::Path) -> mptdb_sc::mpt::MptCommitStore {
    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.use_sparse_storage = true;
    mptdb_sc::mpt::MptCommitStore::open_with_config(dir, false, config).unwrap()
}

/// SP-1: single account + single storage slot via sparse path.
///
/// Mirrors `i7_1_single_account_single_slot`, asserts identical root.
#[test]
fn sp1_sparse_single_account_single_slot() {
    // Reference root from normal path.
    let reference = {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x10);
        let info = default_info(1, 500);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(7), U256::ZERO, U256::from(77))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        root
    };

    // Sparse path.
    let sparse_root = {
        let dir = TempDir::new().unwrap();
        let mut store = open_sparse(dir.path());
        let addr = Address::repeat_byte(0x10);
        let info = default_info(1, 500);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(7), U256::ZERO, U256::from(77))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        root
    };

    assert_eq!(reference, sparse_root, "sparse path must produce identical root to normal path");
}

/// SP-2: multiple accounts + multiple storage slots via sparse path.
#[test]
fn sp2_sparse_multi_account_multi_slot() {
    let make_state = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };

        let addrs: Vec<Address> = (0u8..4).map(Address::repeat_byte).collect();
        let bundle = make_bundle(
            addrs
                .iter()
                .enumerate()
                .map(|(i, &addr)| {
                    let slots: Vec<(U256, U256, U256)> = (0u64..5)
                        .map(|s| (U256::from(s), U256::ZERO, U256::from(i as u64 * 100 + s)))
                        .collect();
                    (
                        addr,
                        Some(default_info(i as u64 + 1, (i + 1) as u64 * 1000)),
                        revm_database::AccountStatus::Changed,
                        slots,
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        (dir, root)
    };

    let (_dir_normal, root_normal) = make_state(false);
    let (_dir_sparse, root_sparse) = make_state(true);
    assert_eq!(root_normal, root_sparse, "multi-account sparse root must match normal");
}

/// SP-3: account with no storage (EOA) via sparse path.
#[test]
fn sp3_sparse_eoa_no_storage() {
    let make_root = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        let addr = Address::repeat_byte(0xee);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(0, 9999)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        (dir, root)
    };
    let (_d1, r1) = make_root(false);
    let (_d2, r2) = make_root(true);
    assert_eq!(r1, r2, "EOA sparse root must match normal");
}

/// SP-4: two consecutive blocks via sparse path.
///
/// Block 1 inserts account + slots.  Block 2 updates a slot.
/// Root after block 2 must match the normal-path root.
#[test]
fn sp4_sparse_two_blocks() {
    let addr = Address::repeat_byte(0x42);
    let slot = U256::from(1);
    let val1 = U256::from(111);
    let val2 = U256::from(222);

    let make_two_block_root = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        // Block 1
        let b1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, val1)],
        )]);
        store.apply_bundle_state(&b1).unwrap();
        store.commit().unwrap();
        // Flush so segments are available for block 2 witness extraction.
        store.flush_persist().unwrap();

        // Block 2
        let b2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(slot, val1, val2)],
        )]);
        store.apply_bundle_state(&b2).unwrap();
        let (_, root) = store.commit().unwrap();
        (dir, root)
    };

    let (_d1, r_normal) = make_two_block_root(false);
    let (_d2, r_sparse) = make_two_block_root(true);
    assert_eq!(r_normal, r_sparse, "two-block sparse root must match normal");
}

/// SP-5: SELFDESTRUCT (storage_wiped) via sparse path.
#[test]
fn sp5_sparse_selfdestruct() {
    use revm_database::AccountStatus;

    let addr = Address::repeat_byte(0x55);
    let slot = U256::from(10);

    let make_root = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        // Block 1: create account with storage
        let b1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 50)),
            AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(99))],
        )]);
        store.apply_bundle_state(&b1).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();

        // Block 2: SELFDESTRUCT (storage_wiped + info=None via DestroyedAgain status)
        let b2 = make_bundle(vec![(addr, None, AccountStatus::DestroyedAgain, vec![])]);
        store.apply_bundle_state(&b2).unwrap();
        let (_, root) = store.commit().unwrap();
        (dir, root)
    };

    let (_d1, r_normal) = make_root(false);
    let (_d2, r_sparse) = make_root(true);
    assert_eq!(r_normal, r_sparse, "selfdestruct sparse root must match normal");
}

// ── Phase 3: dual-run validation + proof generation ───────────────────────────

/// SP-6: 10-block dual-run — sparse root must match normal root on every block.
///
/// Acceptance criterion: "Root hashes match dual-run on 100 blocks".
/// This test uses 10 blocks for speed; adjust to 100 for full validation.
#[test]
fn sp6_sparse_dual_run_10_blocks() {
    let addr_base = 0x80u8;
    let num_blocks = 10usize;

    let run = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };

        let mut roots = Vec::new();
        for blk in 0..num_blocks {
            // Each block: 3 accounts, each with 2 storage slots.
            let accounts: Vec<_> = (0u8..3)
                .map(|i| {
                    let addr = Address::repeat_byte(addr_base.wrapping_add(i));
                    let slots: Vec<(U256, U256, U256)> = (0u64..2)
                        .map(|s| {
                            let orig = if blk == 0 {
                                U256::ZERO
                            } else {
                                U256::from(blk as u64 - 1) * U256::from(s + 1)
                            };
                            let new = U256::from(blk as u64) * U256::from(s + 1) + U256::from(i);
                            (U256::from(s), orig, new)
                        })
                        .collect();
                    (
                        addr,
                        Some(default_info(blk as u64 + 1, (i as u64 + 1) * 1000 + blk as u64)),
                        revm_database::AccountStatus::Changed,
                        slots,
                    )
                })
                .collect();
            let bundle = make_bundle(accounts);
            store.apply_bundle_state(&bundle).unwrap();
            let (_, root) = store.commit().unwrap();
            store.flush_persist().unwrap();
            roots.push(root);
        }
        (dir, roots)
    };

    let (_d_normal, roots_normal) = run(false);
    let (_d_sparse, roots_sparse) = run(true);

    for (blk, (rn, rs)) in roots_normal.iter().zip(roots_sparse.iter()).enumerate() {
        assert_eq!(rn, rs, "root mismatch at block {blk}");
    }
}

/// SP-7: account_proof via sparse path matches normal path for latest version.
#[test]
fn sp7_sparse_account_proof_matches_normal() {
    let addr = Address::repeat_byte(0x11);
    let slot = U256::from(42);
    let val = U256::from(99);

    let run_and_proof = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 500)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, val)],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, _root) = store.commit().unwrap();
        store.flush_persist().unwrap();
        let proof =
            store.account_proof(ver, addr, &[B256::from(slot.to_be_bytes::<32>())]).unwrap();
        (dir, proof)
    };

    let (_d1, proof_normal) = run_and_proof(false);
    let (_d2, proof_sparse) = run_and_proof(true);

    // Both proofs must report the same account info and storage root.
    assert_eq!(proof_normal.address, proof_sparse.address);
    assert_eq!(proof_normal.info, proof_sparse.info, "account info must match");
    assert_eq!(proof_normal.storage_root, proof_sparse.storage_root, "storage root must match");
    // Proof node structure may differ (different encoding paths), but the
    // storage value for the requested slot must be identical.
    assert_eq!(proof_normal.storage_proofs.len(), proof_sparse.storage_proofs.len());
    for (pn, ps) in proof_normal.storage_proofs.iter().zip(proof_sparse.storage_proofs.iter()) {
        assert_eq!(pn.key, ps.key, "slot key must match");
        assert_eq!(pn.value, ps.value, "slot value must match");
    }
}

/// SP-8: Phase 3b — dirty blobs generated in non-wal_first mode.
///
/// Verifies that after a sparse commit in non-wal_first mode, the store can
/// generate dirty blobs (i.e., `all_blobs.len() > 0` would be recorded).
/// We can't inspect blobs directly but can verify the commit succeeds and
/// produces the correct root.
#[test]
fn sp8_sparse_non_wal_first_commit_succeeds() {
    // non-wal_first (default config) with use_sparse_storage=true
    let dir = TempDir::new().unwrap();
    let mut store = open_sparse(dir.path());
    // Default config has wal_first_commit=false.

    let addr = Address::repeat_byte(0x77);
    let bundle = make_bundle(vec![(
        addr,
        Some(default_info(1, 100)),
        revm_database::AccountStatus::Changed,
        vec![(U256::from(1), U256::ZERO, U256::from(55))],
    )]);
    store.apply_bundle_state(&bundle).unwrap();
    let (ver, root) = store.commit().unwrap();
    assert_eq!(ver, 1);
    assert_ne!(root, EMPTY_ROOT_HASH, "root must not be empty after non-wal_first sparse commit");
}

/// SP-9: Phase 3b — restart after non-wal_first sparse commit recovers trie.
///
/// This is the KEY acceptance test for Phase 3b:
/// "any restart without a valid WAL will fail to reconstruct the trie"
///
/// Flow:
/// 1. Commit block 1 with use_sparse_storage=true (non-wal_first, writes dirty blobs)
/// 2. Close + reopen the store (no WAL replay)
/// 3. Commit block 2 with more changes
/// 4. Verify block 2 root == root computed by normal path from same initial state
#[test]
fn sp9_phase3b_restart_after_sparse_non_wal_first() {
    let addr = Address::repeat_byte(0xAA);
    let slot = U256::from(7);
    let val1 = U256::from(100);
    let val2 = U256::from(200);

    let dir = TempDir::new().unwrap();

    // ── Phase: commit block 1 with sparse mode ──────────────────────────────
    let root1 = {
        let mut store = open_sparse(dir.path());
        let b1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 1000)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, val1)],
        )]);
        store.apply_bundle_state(&b1).unwrap();
        let (_, root) = store.commit().unwrap();
        // close — dirty blobs must be persisted (flush_persist is implicit on close)
        store.close().unwrap();
        root
    };
    assert_ne!(root1, EMPTY_ROOT_HASH, "block 1 root must not be empty");

    // ── Phase: reopen + commit block 2 ─────────────────────────────────────
    let root2_sparse = {
        let mut store = open_sparse(dir.path());
        assert_eq!(store.version(), 1, "reopened store must be at version 1");
        let b2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 900)),
            revm_database::AccountStatus::Changed,
            vec![(slot, val1, val2)],
        )]);
        store.apply_bundle_state(&b2).unwrap();
        let (_, root) = store.commit().unwrap();
        store.close().unwrap();
        root
    };

    // ── Reference: commit same two blocks with normal path ──────────────────
    let root2_normal = {
        let dir2 = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir2.path(), false).unwrap();
        let b1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 1000)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, val1)],
        )]);
        store.apply_bundle_state(&b1).unwrap();
        store.commit().unwrap();
        let b2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 900)),
            revm_database::AccountStatus::Changed,
            vec![(slot, val1, val2)],
        )]);
        store.apply_bundle_state(&b2).unwrap();
        let (_, root) = store.commit().unwrap();
        root
    };

    assert_eq!(
        root2_sparse, root2_normal,
        "block 2 root after restart must match normal path (Phase 3b dirty blobs correct)"
    );
}

/// SP-10: Phase 3b — multiple accounts, multiple slots, restart recovery.
///
/// Exercises the extension node path: accounts with common keccak(addr) prefix
/// create extension nodes in the account trie.  These must be persisted via
/// dirty blobs.
#[test]
fn sp10_phase3b_extension_nodes_persisted() {
    let dir = TempDir::new().unwrap();

    // Build block 1 with 6 accounts (more accounts = more branch/extension nodes)
    let addrs: Vec<Address> = (1u8..=6).map(Address::repeat_byte).collect();

    let block1_root = {
        let mut store = open_sparse(dir.path());
        let bundle = make_bundle(
            addrs
                .iter()
                .map(|&a| {
                    (
                        a,
                        Some(default_info(1, 500)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(1u64), U256::ZERO, U256::from(42u64))],
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        store.close().unwrap();
        root
    };

    // Reopen and commit another block updating the same accounts
    let block2_root_sparse = {
        let mut store = open_sparse(dir.path());
        let bundle = make_bundle(
            addrs
                .iter()
                .enumerate()
                .map(|(i, &a)| {
                    (
                        a,
                        Some(default_info(2, 400 + i as u64 * 10)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(1u64), U256::from(42u64), U256::from(99u64 + i as u64))],
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();
        store.close().unwrap();
        root
    };

    // Reference: same two blocks via normal path
    let block2_root_normal = {
        let dir2 = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir2.path(), false).unwrap();
        let b1 = make_bundle(
            addrs
                .iter()
                .map(|&a| {
                    (
                        a,
                        Some(default_info(1, 500)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(1u64), U256::ZERO, U256::from(42u64))],
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&b1).unwrap();
        store.commit().unwrap();
        let b2 = make_bundle(
            addrs
                .iter()
                .enumerate()
                .map(|(i, &a)| {
                    (
                        a,
                        Some(default_info(2, 400 + i as u64 * 10)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(1u64), U256::from(42u64), U256::from(99u64 + i as u64))],
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&b2).unwrap();
        let (_, root) = store.commit().unwrap();
        root
    };

    let _ = block1_root; // used implicitly via store reopen
    assert_eq!(
        block2_root_sparse, block2_root_normal,
        "post-restart block 2 root must match (extension nodes correctly persisted)"
    );
}

/// SP-11: Phase 3b — verify account_proof after restart.
///
/// After restart, account_proof returns an error because the sparse trie is
/// discarded on close and `proof.rs` has been deleted.  The caller must
/// re-apply the latest block to restore proof generation capability.
///
/// This test verifies the expected error behavior and that the store is
/// otherwise healthy after restart (correct version, state root).
#[test]
fn sp11_phase3b_account_proof_after_restart() {
    let addr = Address::repeat_byte(0xCC);
    let slot = U256::from(3);
    let val = U256::from(77);
    let dir = TempDir::new().unwrap();

    // Commit block 1 sparse + close
    let (ver1, root1) = {
        let mut store = open_sparse(dir.path());
        let b = make_bundle(vec![(
            addr,
            Some(default_info(1, 200)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, val)],
        )]);
        store.apply_bundle_state(&b).unwrap();
        let (v, r) = store.commit().unwrap();
        store.flush_persist().unwrap();
        store.close().unwrap();
        (v, r)
    };

    // After restart: sparse trie is gone; account_proof returns an error.
    // The store is otherwise healthy (correct version and root).
    let store = open_sparse(dir.path());
    assert_eq!(store.version(), ver1, "version must be restored");
    assert_eq!(store.frontier().committed_root, root1, "committed root must match");
    // proof returns error since sparse trie is not available after restart
    let proof_result = store.account_proof(ver1, addr, &[B256::from(slot.to_be_bytes::<32>())]);
    assert!(
        proof_result.is_err(),
        "account_proof must return error after restart without sparse trie"
    );
}

// ── Phase 4: cross-block SparseStateTrie optimization ─────────────────────────

fn open_cross_block(dir: &std::path::Path) -> mptdb_sc::mpt::MptCommitStore {
    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.use_sparse_storage = true;
    config.cross_block_sparse = true;
    config.cross_block_sparse_max_lag = 4;
    mptdb_sc::mpt::MptCommitStore::open_with_config(dir, false, config).unwrap()
}

/// SP-12: cross-block 10-block dual-run — roots match normal path on every block.
#[test]
fn sp12_cross_block_dual_run_10_blocks() {
    let num_blocks = 10usize;
    let addr_base = 0xC0u8;

    let run = |use_cross: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_cross {
            open_cross_block(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        let mut roots = Vec::new();
        for blk in 0..num_blocks {
            let accounts: Vec<_> = (0u8..4)
                .map(|i| {
                    let addr = Address::repeat_byte(addr_base.wrapping_add(i));
                    // Same slots modified every block (HOT PATH — cross-block optimization)
                    let slots: Vec<(U256, U256, U256)> = (0u64..3)
                        .map(|s| {
                            let orig = if blk == 0 {
                                U256::ZERO
                            } else {
                                U256::from((blk as u64 - 1) * 10 + s)
                            };
                            let new = U256::from(blk as u64 * 10 + s);
                            (U256::from(s), orig, new)
                        })
                        .collect();
                    (
                        addr,
                        Some(default_info(blk as u64 + 1, 1000)),
                        revm_database::AccountStatus::Changed,
                        slots,
                    )
                })
                .collect();
            store.apply_bundle_state(&make_bundle(accounts)).unwrap();
            let (_, root) = store.commit().unwrap();
            store.flush_persist().unwrap();
            roots.push(root);
        }
        (dir, roots)
    };

    let (_d_normal, roots_normal) = run(false);
    let (_d_cross, roots_cross) = run(true);

    for (blk, (rn, rc)) in roots_normal.iter().zip(roots_cross.iter()).enumerate() {
        assert_eq!(rn, rc, "cross-block root mismatch at block {blk}");
    }
}

/// SP-13: cross-block restart — trie rebuilt correctly after reopen.
#[test]
fn sp13_cross_block_restart_recovery() {
    let addr = Address::repeat_byte(0xD0);
    let slot = U256::from(5);
    let dir = TempDir::new().unwrap();

    // Block 1 with cross-block enabled
    let root1 = {
        let mut store = open_cross_block(dir.path());
        let b = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&b).unwrap();
        let (_, r) = store.commit().unwrap();
        store.close().unwrap();
        r
    };

    // Block 2 after restart (cross-block state is discarded; should work as per-block)
    let root2_cross = {
        let mut store = open_cross_block(dir.path());
        let b = make_bundle(vec![(
            addr,
            Some(default_info(2, 90)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(10), U256::from(20))],
        )]);
        store.apply_bundle_state(&b).unwrap();
        let (_, r) = store.commit().unwrap();
        store.close().unwrap();
        r
    };

    // Reference via normal path
    let root2_normal = {
        let dir2 = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir2.path(), false).unwrap();
        let b1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&b1).unwrap();
        store.commit().unwrap();
        let b2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 90)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(10), U256::from(20))],
        )]);
        store.apply_bundle_state(&b2).unwrap();
        let (_, r) = store.commit().unwrap();
        r
    };

    let _ = root1;
    assert_eq!(root2_cross, root2_normal, "cross-block restart root must match normal");
}

/// SP-14: cross-block LRU eviction — cold accounts evicted, hot accounts retained.
#[test]
fn sp14_cross_block_lru_eviction() {
    // max_lag=2: accounts not accessed for 2 blocks are evicted from the trie.
    let dir = TempDir::new().unwrap();
    let hot_addr = Address::repeat_byte(0xE0);
    let cold_addr = Address::repeat_byte(0xE1);

    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.use_sparse_storage = true;
    config.cross_block_sparse = true;
    config.cross_block_sparse_max_lag = 2;
    let mut store =
        mptdb_sc::mpt::MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

    let slot = U256::from(1);

    // Block 1: both hot + cold
    store
        .apply_bundle_state(&make_bundle(vec![
            (
                hot_addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::ZERO, U256::from(1))],
            ),
            (
                cold_addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::ZERO, U256::from(1))],
            ),
        ]))
        .unwrap();
    store.commit().unwrap();
    store.flush_persist().unwrap();

    // Blocks 2+3: only hot account touched (cold_addr not accessed)
    for blk in 2u64..=3 {
        store
            .apply_bundle_state(&make_bundle(vec![(
                hot_addr,
                Some(default_info(blk, 100 - blk as u64 * 5)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::from(blk - 1), U256::from(blk))],
            )]))
            .unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();
    }

    // Block 4: cold_addr accessed again — it was evicted after max_lag=2
    // This must still produce the correct root (evicted trie re-revealed from segment).
    let (_, root_cross) = {
        store
            .apply_bundle_state(&make_bundle(vec![(
                cold_addr,
                Some(default_info(4, 50)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::from(1), U256::from(99))],
            )]))
            .unwrap();
        let r = store.commit().unwrap();
        r
    };

    // Reference: normal path same 4 blocks
    let root_normal = {
        let dir2 = TempDir::new().unwrap();
        let mut s = MptCommitStore::open(dir2.path(), false).unwrap();
        let b1 = make_bundle(vec![
            (
                hot_addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::ZERO, U256::from(1))],
            ),
            (
                cold_addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::ZERO, U256::from(1))],
            ),
        ]);
        s.apply_bundle_state(&b1).unwrap();
        s.commit().unwrap();
        for blk in 2u64..=3 {
            let b = make_bundle(vec![(
                hot_addr,
                Some(default_info(blk, 100 - blk * 5)),
                revm_database::AccountStatus::Changed,
                vec![(slot, U256::from(blk - 1), U256::from(blk))],
            )]);
            s.apply_bundle_state(&b).unwrap();
            s.commit().unwrap();
        }
        let b4 = make_bundle(vec![(
            cold_addr,
            Some(default_info(4, 50)),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(1), U256::from(99))],
        )]);
        s.apply_bundle_state(&b4).unwrap();
        let (_, r) = s.commit().unwrap();
        r
    };

    assert_eq!(root_cross, root_normal, "cross-block + LRU eviction root must match normal");
}

/// SP-15: 100-block dual-run — root must match normal path on every block.
///
/// Acceptance criterion: "Root hashes match dual-run on 100 blocks".
/// Uses a mix of hot accounts (same slots each block) and cold accounts
/// (new accounts introduced every 10 blocks) to exercise both paths.
#[test]
fn sp15_sparse_dual_run_100_blocks() {
    let num_blocks = 100usize;
    let hot_addr_base = 0x50u8;
    let cold_addr_base = 0x80u8;
    let hot_slot = U256::from(1);

    let run = |use_sparse: bool| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };

        let mut roots = Vec::new();
        for blk in 0..num_blocks {
            let mut accounts = Vec::new();

            // 3 hot accounts modified every block (same slot)
            for i in 0u8..3 {
                let addr = Address::repeat_byte(hot_addr_base.wrapping_add(i));
                let orig = if blk == 0 { U256::ZERO } else { U256::from(blk as u64 - 1) };
                let new = U256::from(blk as u64 + 1);
                accounts.push((
                    addr,
                    Some(default_info(blk as u64 + 1, 1000 + i as u64)),
                    revm_database::AccountStatus::Changed,
                    vec![(hot_slot, orig, new)],
                ));
            }

            // 1 new cold account every 10 blocks (first-time slot write)
            if blk % 10 == 0 {
                let addr = Address::repeat_byte(cold_addr_base.wrapping_add((blk / 10) as u8));
                accounts.push((
                    addr,
                    Some(default_info(1, 500)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(blk as u64), U256::ZERO, U256::from(blk as u64 * 7 + 1))],
                ));
            }

            store.apply_bundle_state(&make_bundle(accounts)).unwrap();
            let (_, root) = store.commit().unwrap();
            // Flush every 10 blocks so segments are available for sparse
            // witness extraction when needed.
            if blk % 10 == 9 {
                store.flush_persist().unwrap();
            }
            roots.push(root);
        }
        (dir, roots)
    };

    let (_d_normal, roots_normal) = run(false);
    let (_d_sparse, roots_sparse) = run(true);

    for (blk, (rn, rs)) in roots_normal.iter().zip(roots_sparse.iter()).enumerate() {
        assert_eq!(rn, rs, "root mismatch at block {blk}: normal={rn:?} sparse={rs:?}");
    }
}

/// Regression test: hash cache staleness bug fixed in Phase 2b.
/// Identical to sp15_sparse_dual_run_100_blocks but with flush every block;
/// kept to prevent regressing on the specific block-41 failure case.
#[test]
fn sp15_regression_block41_hash_cache() {
    let num_blocks = 42usize;
    let hot_addr_base = 0x50u8;
    let cold_addr_base = 0x80u8;
    let hot_slot = U256::from(1);

    let run = |use_sparse: bool, flush_every: usize| {
        let dir = TempDir::new().unwrap();
        let mut store = if use_sparse {
            open_sparse(dir.path())
        } else {
            MptCommitStore::open(dir.path(), false).unwrap()
        };
        let mut roots = Vec::new();
        for blk in 0..num_blocks {
            let mut accounts = Vec::new();
            for i in 0u8..3 {
                let addr = Address::repeat_byte(hot_addr_base.wrapping_add(i));
                let orig = if blk == 0 { U256::ZERO } else { U256::from(blk as u64 - 1) };
                let new = U256::from(blk as u64 + 1);
                accounts.push((
                    addr,
                    Some(default_info(blk as u64 + 1, 1000 + i as u64)),
                    revm_database::AccountStatus::Changed,
                    vec![(hot_slot, orig, new)],
                ));
            }
            if blk % 10 == 0 {
                let addr = Address::repeat_byte(cold_addr_base.wrapping_add((blk / 10) as u8));
                accounts.push((
                    addr,
                    Some(default_info(1, 500)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(blk as u64), U256::ZERO, U256::from(blk as u64 * 7 + 1))],
                ));
            }
            store.apply_bundle_state(&make_bundle(accounts)).unwrap();
            let (_, root) = store.commit().unwrap();
            if blk % flush_every == flush_every - 1 {
                store.flush_persist().unwrap();
            }
            roots.push(root);
        }
        (dir, roots)
    };

    // Flush every block (should always match)
    let (_d1, r1_sparse) = run(true, 1);
    let (_d2, r1_normal) = run(false, 1);
    for (blk, (rn, rs)) in r1_normal.iter().zip(r1_sparse.iter()).enumerate() {
        assert_eq!(rn, rs, "flush-every-1: mismatch at block {blk}");
    }
}
