//! Phase 1b acceptance tests for MptDbStateProvider / MptDbStateWriter.

use crate::{MptDbStateProvider, MptDbStateWriter};
use alloy_primitives::{Address, U256};
use mptdb_common::config::StateStoreConfig;
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
use mptdb_ss::{evm::store::EVMStateStore, factory::new_state_store};
use parking_lot::Mutex;
use reth_execution_types::ExecutionOutcome;
use reth_storage_api::{
    AccountReader, HashedPostStateProvider, StateProvider, StateRootProvider, StateWriteConfig,
    StateWriter,
};
use reth_trie_common::HashedPostState;
use revm_database::{
    states::StorageSlot, AccountStatus, BundleAccount, BundleState, OriginalValuesKnown,
};
use revm_state::AccountInfo;
use std::sync::Arc;
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_sc(dir: &std::path::Path) -> MptCommitStore {
    MptCommitStore::open(dir, false).unwrap()
}

fn open_ss(dir: &std::path::Path) -> Arc<EVMStateStore> {
    let config = StateStoreConfig {
        db_directory: dir.join("ss").to_string_lossy().to_string(),
        keep_last_version: true,
        ..Default::default()
    };
    new_state_store(&config, &dir.to_string_lossy()).unwrap()
}

fn noop_fallback() -> Arc<dyn StateProvider + Send + Sync> {
    Arc::new(reth_storage_api::noop::NoopProvider::default())
}

fn noop_block_id() -> Arc<dyn reth_storage_api::BlockIdReader + Send + Sync> {
    Arc::new(reth_storage_api::noop::NoopProvider::default())
}

fn make_bundle(
    address: Address,
    nonce: u64,
    balance: u64,
    storage: Vec<(U256, U256)>,
) -> BundleState {
    let info = AccountInfo {
        nonce,
        balance: U256::from(balance),
        code_hash: alloy_trie::KECCAK_EMPTY,
        account_id: None,
        code: None,
    };
    let storage_map: revm_database::StorageWithOriginalValues = storage
        .into_iter()
        .map(|(slot, val)| (slot, StorageSlot::new_changed(U256::ZERO, val)))
        .collect();
    let account = BundleAccount::new(None, Some(info), storage_map, AccountStatus::Loaded);
    let mut state = alloy_primitives::map::HashMap::default();
    state.insert(address, account);
    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

/// Write a block and return the committed state root.
/// Uses apply_changeset_sync so SS data is readable immediately after return.
fn write_block_get_root(
    sc: &Arc<Mutex<MptCommitStore>>,
    ss: &Arc<EVMStateStore>,
    bundle: &BundleState,
    block_number: u64,
) -> alloy_primitives::B256 {
    let root = {
        let mut guard = sc.lock();
        guard.apply_bundle_state(bundle).unwrap();
        let (_, r) = guard.commit().unwrap();
        r
    };
    use mptdb_sc::mpt::ss_changeset::bundle_to_ss_changeset;
    use mptdb_traits::ss::StateStore as _;
    let cs = bundle_to_ss_changeset(bundle);
    // SS version = block_number + 1 (avoids MVCC version-0 fixup).
    ss.apply_changeset_sync(block_number as i64 + 1, &cs).unwrap();
    root
}

fn make_provider(
    sc: Arc<Mutex<MptCommitStore>>,
    ss: Arc<EVMStateStore>,
    version: i64,
) -> MptDbStateProvider {
    MptDbStateProvider::new(ss, sc, version, noop_fallback(), noop_block_id())
}

// Not needed — write_block_get_root uses apply_changeset_sync.

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Version mapping discovery: SS MVCC treats version=0 as version=1 internally
/// (see mptdb-engine/src/mvcc/write.rs:17). So:
///   block 0 written at SS version 0 → internally stored at version 1
///   block 1 written at SS version 1 → stored at version 1 (same slot!)
/// This means the plan's "version = block_number as i64" needs adjustment:
///   SS read/write version should be (block_number + 1) to avoid the version-0 fixup.
///
/// After block 0, SC version == 1 (§5.4 mapping).
#[test]
#[ignore]
fn write_state_sc_version_mapping() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let bundle = make_bundle(Address::repeat_byte(0x01), 7, 1000, vec![]);

    assert_eq!(sc.lock().version(), 0);
    write_block_get_root(&sc, &ss, &bundle, 0);
    assert_eq!(sc.lock().version(), 1, "SC version 1 = block 0");
}

/// Helper: correct SS version for reading block N's state.
/// SS MVCC skips version 0 (treats it as 1), so use block_number + 1 for SS.
fn ss_version_for_block(block_number: u64) -> i64 {
    (block_number + 1) as i64
}

/// state_root(HashedPostState) dry-run == SC commit root for same block.
#[test]
#[ignore]
fn state_root_dry_run_matches_commit_root() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let addr = Address::repeat_byte(0x02);
    let bundle = make_bundle(addr, 3, 500, vec![(U256::from(1u64), U256::from(42u64))]);

    // Compute HashedPostState from bundle (same as reth does before state_root call)
    let hps: HashedPostState = {
        use alloy_primitives::keccak256;
        use reth_trie_common::HashedStorage;
        let mut h = HashedPostState::default();
        for (address, account) in &bundle.state {
            let haddr = keccak256(address.as_slice());
            h.accounts.insert(haddr, account.info.as_ref().map(|i| i.into()));
            let storage = HashedStorage::from_plain_storage(
                account.status,
                account.storage.iter().map(|(s, v)| (s, &v.present_value)),
            );
            if !storage.is_empty() {
                h.storages.insert(haddr, storage);
            }
        }
        h
    };

    // Dry-run root (SC not yet committed)
    let provider = make_provider(Arc::clone(&sc), Arc::clone(&ss), ss_version_for_block(0));
    let root_dry = provider.state_root(hps).unwrap();

    // Commit root
    let root_commit = write_block_get_root(&sc, &ss, &bundle, 0);

    assert_eq!(root_dry, root_commit, "dry-run root must match commit root");
}

/// basic_account reads back correct values after write.
#[test]
#[ignore]
fn basic_account_reads_back() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let addr = Address::repeat_byte(0x03);
    let bundle = make_bundle(addr, 5, 999, vec![]);

    write_block_get_root(&sc, &ss, &bundle, 0);

    let provider = make_provider(sc, ss, ss_version_for_block(0));
    let account = provider.basic_account(&addr).unwrap().expect("account must exist");
    assert_eq!(account.nonce, 5);
    assert_eq!(account.balance, U256::from(999u64));
}

/// storage() reads back correct value after write.
#[test]
#[ignore]
fn storage_reads_back() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let addr = Address::repeat_byte(0x04);
    let slot = U256::from(7u64);
    let value = U256::from(123u64);
    let bundle = make_bundle(addr, 1, 0, vec![(slot, value)]);

    write_block_get_root(&sc, &ss, &bundle, 0);

    let provider = make_provider(sc, ss, ss_version_for_block(0));
    let slot_key = alloy_primitives::StorageKey::from(slot.to_be_bytes::<32>());
    let stored = provider.storage(addr, slot_key).unwrap().expect("slot must exist");
    assert_eq!(stored, value);
}

/// Consecutive blocks accumulate correctly; version 0 sees block 0 state.
#[test]
#[ignore]
fn consecutive_blocks() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let addr = Address::repeat_byte(0x05);

    write_block_get_root(&sc, &ss, &make_bundle(addr, 1, 100, vec![]), 0);
    write_block_get_root(&sc, &ss, &make_bundle(addr, 2, 200, vec![]), 1);

    assert_eq!(sc.lock().version(), 2);

    let p0 = make_provider(Arc::clone(&sc), Arc::clone(&ss), ss_version_for_block(0));
    assert_eq!(p0.basic_account(&addr).unwrap().unwrap().nonce, 1);

    let p1 = make_provider(Arc::clone(&sc), Arc::clone(&ss), ss_version_for_block(1));
    assert_eq!(p1.basic_account(&addr).unwrap().unwrap().nonce, 2);
}

/// Stub paths return Err, not panic.
#[test]
#[ignore]
fn stub_paths_return_err_not_panic() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let ss = open_ss(dir.path());
    let provider = make_provider(sc, ss, ss_version_for_block(0));

    use reth_storage_api::{StateRootProvider, StorageRootProvider};
    use reth_trie_common::TrieInput;

    assert!(provider.state_root_from_nodes(TrieInput::default()).is_err());
    assert!(provider.state_root_from_nodes_with_updates(TrieInput::default()).is_err());
    assert!(provider.storage_root(Address::ZERO, Default::default()).is_err());

    use reth_storage_api::StateProofProvider;
    assert!(provider.proof(TrieInput::default(), Address::ZERO, &[]).is_err());
    assert!(provider.multiproof(TrieInput::default(), Default::default()).is_err());
    assert!(provider.witness(TrieInput::default(), Default::default()).is_err());
}
