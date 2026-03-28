//! Plan C acceptance tests for MptDbStateProvider / MptDbStateWriter.
//!
//! All tests use a typed in-memory `MapStateProvider` as the fallback,
//! mirroring the Plan C architecture where basic_account / storage reads come
//! from reth's PlainState (MDBX) and SC handles state_root / proof only.
//! No SS (mptdb-ss) references appear here.

use crate::{MptDbStateProvider, MptDbStateWriter};
use alloy_primitives::{Address, U256};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
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
use std::{collections::HashMap, sync::Arc};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_sc(dir: &std::path::Path) -> MptCommitStore {
    MptCommitStore::open(dir, false).unwrap()
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

// ── MapStateProvider — typed in-memory fallback (replaces MDBX in tests) ────

struct MapStateProvider {
    accounts: HashMap<Address, AccountInfo>,
    storage: HashMap<Address, HashMap<U256, U256>>,
}

impl MapStateProvider {
    fn empty() -> Arc<Self> {
        Arc::new(Self { accounts: HashMap::new(), storage: HashMap::new() })
    }

    fn with_account(addr: Address, nonce: u64, balance: u64) -> Arc<Self> {
        let mut accounts = HashMap::new();
        accounts.insert(
            addr,
            AccountInfo {
                nonce,
                balance: U256::from(balance),
                code_hash: alloy_trie::KECCAK_EMPTY,
                code: None,
                account_id: None,
            },
        );
        Arc::new(Self { accounts, storage: HashMap::new() })
    }

    fn with_account_and_storage(
        addr: Address,
        nonce: u64,
        balance: u64,
        slots: Vec<(U256, U256)>,
    ) -> Arc<Self> {
        let mut accounts = HashMap::new();
        accounts.insert(
            addr,
            AccountInfo {
                nonce,
                balance: U256::from(balance),
                code_hash: alloy_trie::KECCAK_EMPTY,
                code: None,
                account_id: None,
            },
        );
        let mut storage = HashMap::new();
        storage.insert(addr, slots.into_iter().collect());
        Arc::new(Self { accounts, storage })
    }
}

// Sync is auto-derived: HashMap<Address, AccountInfo> is Sync,
// HashMap<Address, HashMap<U256, U256>> is Sync.

impl AccountReader for MapStateProvider {
    fn basic_account(
        &self,
        address: &Address,
    ) -> reth_storage_api::errors::provider::ProviderResult<Option<reth_primitives_traits::Account>>
    {
        Ok(self.accounts.get(address).map(|i| reth_primitives_traits::Account {
            nonce: i.nonce,
            balance: i.balance,
            bytecode_hash: if i.code_hash == alloy_trie::KECCAK_EMPTY {
                None
            } else {
                Some(i.code_hash)
            },
        }))
    }
}

impl reth_storage_api::BlockHashReader for MapStateProvider {
    fn block_hash(
        &self,
        _: u64,
    ) -> reth_storage_api::errors::provider::ProviderResult<Option<alloy_primitives::B256>> {
        Ok(None)
    }
    fn canonical_hashes_range(
        &self,
        _: u64,
        _: u64,
    ) -> reth_storage_api::errors::provider::ProviderResult<Vec<alloy_primitives::B256>> {
        Ok(vec![])
    }
}
impl reth_storage_api::BytecodeReader for MapStateProvider {
    fn bytecode_by_hash(
        &self,
        _: &alloy_primitives::B256,
    ) -> reth_storage_api::errors::provider::ProviderResult<Option<reth_primitives_traits::Bytecode>>
    {
        Ok(None)
    }
}
impl StateRootProvider for MapStateProvider {
    fn state_root(
        &self,
        _: HashedPostState,
    ) -> reth_storage_api::errors::provider::ProviderResult<alloy_primitives::B256> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn state_root_from_nodes(
        &self,
        _: reth_trie_common::TrieInput,
    ) -> reth_storage_api::errors::provider::ProviderResult<alloy_primitives::B256> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn state_root_with_updates(
        &self,
        _: HashedPostState,
    ) -> reth_storage_api::errors::provider::ProviderResult<(
        alloy_primitives::B256,
        reth_trie_common::updates::TrieUpdates,
    )> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn state_root_from_nodes_with_updates(
        &self,
        _: reth_trie_common::TrieInput,
    ) -> reth_storage_api::errors::provider::ProviderResult<(
        alloy_primitives::B256,
        reth_trie_common::updates::TrieUpdates,
    )> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
}
impl reth_storage_api::StorageRootProvider for MapStateProvider {
    fn storage_root(
        &self,
        _: Address,
        _: reth_trie_common::HashedStorage,
    ) -> reth_storage_api::errors::provider::ProviderResult<alloy_primitives::B256> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn storage_proof(
        &self,
        _: Address,
        _: alloy_primitives::B256,
        _: reth_trie_common::HashedStorage,
    ) -> reth_storage_api::errors::provider::ProviderResult<reth_trie_common::StorageProof> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn storage_multiproof(
        &self,
        _: Address,
        _: &[alloy_primitives::B256],
        _: reth_trie_common::HashedStorage,
    ) -> reth_storage_api::errors::provider::ProviderResult<reth_trie_common::StorageMultiProof>
    {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
}
impl reth_storage_api::StateProofProvider for MapStateProvider {
    fn proof(
        &self,
        _: reth_trie_common::TrieInput,
        _: Address,
        _: &[alloy_primitives::B256],
    ) -> reth_storage_api::errors::provider::ProviderResult<reth_trie_common::AccountProof> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn multiproof(
        &self,
        _: reth_trie_common::TrieInput,
        _: reth_trie_common::MultiProofTargets,
    ) -> reth_storage_api::errors::provider::ProviderResult<reth_trie_common::MultiProof> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
    fn witness(
        &self,
        _: reth_trie_common::TrieInput,
        _: HashedPostState,
    ) -> reth_storage_api::errors::provider::ProviderResult<Vec<alloy_primitives::Bytes>> {
        Err(reth_storage_api::errors::provider::ProviderError::Database(
            reth_storage_api::errors::db::DatabaseError::Other("not supported".into()),
        ))
    }
}
impl HashedPostStateProvider for MapStateProvider {
    fn hashed_post_state(&self, _: &BundleState) -> HashedPostState {
        HashedPostState::default()
    }
}
impl StateProvider for MapStateProvider {
    fn storage(
        &self,
        addr: Address,
        key: alloy_primitives::StorageKey,
    ) -> reth_storage_api::errors::provider::ProviderResult<Option<alloy_primitives::StorageValue>>
    {
        let slot = U256::from_be_bytes(*key);
        Ok(self.storage.get(&addr).and_then(|m| m.get(&slot)).copied())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Plan C: basic_account is served by the fallback provider, not SC.
#[test]
fn plan_c_basic_account_reads_from_fallback() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let addr = Address::repeat_byte(0xAA);

    // Commit a block to SC (advances SC version).
    let bundle = make_bundle(addr, 7, 1000, vec![]);
    {
        let mut guard = sc.lock();
        guard.apply_bundle_state(&bundle).unwrap();
        guard.commit().unwrap();
    }

    // Fallback has different data — Plan C: basic_account must return fallback data.
    let fallback = MapStateProvider::with_account(addr, 99, 42);
    let version = sc.lock().version();
    let provider = MptDbStateProvider::new(Arc::clone(&sc), version, fallback, noop_block_id());

    let account = provider.basic_account(&addr).unwrap().expect("account must exist");
    assert_eq!(account.nonce, 99, "nonce must come from fallback, not SC");
    assert_eq!(account.balance, U256::from(42u64), "balance must come from fallback");
}

/// Plan C: storage() is served by the fallback provider.
#[test]
fn plan_c_storage_reads_from_fallback() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let addr = Address::repeat_byte(0xBB);
    let slot = U256::from(7u64);
    let value = U256::from(123u64);

    let bundle = make_bundle(addr, 1, 0, vec![(slot, value)]);
    {
        let mut guard = sc.lock();
        guard.apply_bundle_state(&bundle).unwrap();
        guard.commit().unwrap();
    }

    // Fallback has the storage data.
    let fallback = MapStateProvider::with_account_and_storage(addr, 1, 0, vec![(slot, value)]);
    let version = sc.lock().version();
    let provider = MptDbStateProvider::new(Arc::clone(&sc), version, fallback, noop_block_id());

    let slot_key = alloy_primitives::StorageKey::from(slot.to_be_bytes::<32>());
    let stored = provider.storage(addr, slot_key).unwrap().expect("slot must exist");
    assert_eq!(stored, value);
}

/// Plan C: state_root is computed by SC (dry-run overlay), not fallback.
#[test]
fn plan_c_state_root_uses_sc() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let addr = Address::repeat_byte(0xCC);
    let bundle = make_bundle(addr, 3, 500, vec![(U256::from(1u64), U256::from(42u64))]);

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

    // Dry-run root before commit.
    let version = sc.lock().version();
    let provider = MptDbStateProvider::new(
        Arc::clone(&sc),
        version,
        MapStateProvider::empty(),
        noop_block_id(),
    );
    let root_dry = provider.state_root(hps).unwrap();

    // Commit.
    let mut guard = sc.lock();
    guard.apply_bundle_state(&bundle).unwrap();
    let (_, root_commit) = guard.commit().unwrap();

    assert_eq!(root_dry, root_commit, "Plan C state_root dry-run must match committed root");
}

/// Plan C: write_state only writes to SC; reads come from fallback.
#[test]
fn plan_c_write_state_sc_only_read_from_fallback() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let addr = Address::repeat_byte(0xDD);

    let writer = MptDbStateWriter::<reth_ethereum_primitives::Receipt>::new(Arc::clone(&sc));
    let bundle = make_bundle(addr, 5, 999, vec![]);
    writer
        .write_state(
            &ExecutionOutcome::new(bundle.clone(), Default::default(), 1, Default::default()),
            OriginalValuesKnown::Yes,
            StateWriteConfig::default(),
        )
        .unwrap();

    assert_eq!(sc.lock().version(), 1, "SC must advance after write_state");

    // Fallback has different data — reads must come from fallback, not SC.
    let fallback = MapStateProvider::with_account(addr, 77, 888);
    let version = sc.lock().version();
    let provider = MptDbStateProvider::new(Arc::clone(&sc), version, fallback, noop_block_id());
    let account = provider.basic_account(&addr).unwrap().expect("account must exist");
    assert_eq!(account.nonce, 77, "Plan C: reads come from fallback, not SC");
}

/// SC version advances correctly: block 0 → version 1.
#[test]
fn sc_version_mapping_after_write_state() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    assert_eq!(sc.lock().version(), 0, "fresh SC starts at version 0");

    let writer = MptDbStateWriter::<reth_ethereum_primitives::Receipt>::new(Arc::clone(&sc));
    let bundle = make_bundle(Address::repeat_byte(0x01), 7, 1000, vec![]);
    writer
        .write_state(
            &ExecutionOutcome::new(bundle, Default::default(), 1, Default::default()),
            OriginalValuesKnown::Yes,
            StateWriteConfig::default(),
        )
        .unwrap();

    assert_eq!(sc.lock().version(), 1, "SC version 1 = block 0 committed");
}

/// remove_state_above (reorg rollback) rolls SC back to the target version.
#[test]
fn remove_state_above_rolls_back_sc() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let writer = MptDbStateWriter::<reth_ethereum_primitives::Receipt>::new(Arc::clone(&sc));

    // Commit 3 blocks.
    for blk in 0..3u64 {
        let bundle = make_bundle(Address::repeat_byte(0x10 + blk as u8), blk + 1, 100, vec![]);
        writer
            .write_state(
                &ExecutionOutcome::new(bundle, Default::default(), blk + 1, Default::default()),
                OriginalValuesKnown::Yes,
                StateWriteConfig::default(),
            )
            .unwrap();
    }
    assert_eq!(sc.lock().version(), 3);

    // Roll back to block 1 (SC version 2 = after block 1).
    use reth_storage_api::StateWriter as _;
    writer.remove_state_above(1).unwrap();
    assert_eq!(sc.lock().version(), 2, "SC must be at version 2 (= block 1 + 1) after rollback");
}

/// Stub paths return Err, not panic.
#[test]
fn stub_paths_return_err_not_panic() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let provider =
        MptDbStateProvider::new(Arc::clone(&sc), 0, MapStateProvider::empty(), noop_block_id());

    use reth_storage_api::{StateProofProvider, StateRootProvider};
    use reth_trie_common::TrieInput;

    // These are truly unimplemented stubs — must return Err.
    assert!(provider.state_root_from_nodes(TrieInput::default()).is_err());
    assert!(provider.state_root_from_nodes_with_updates(TrieInput::default()).is_err());
    assert!(provider.multiproof(TrieInput::default(), Default::default()).is_err());
    assert!(provider.witness(TrieInput::default(), Default::default()).is_err());
    // proof / storage_root delegate to SC and return Ok for a fresh (empty) SC.
}

/// proof() succeeds for a committed account.
#[test]
fn proof_succeeds_for_committed_account() {
    let dir = TempDir::new().unwrap();
    let sc = Arc::new(Mutex::new(open_sc(dir.path())));
    let addr = Address::repeat_byte(0xEF);
    let bundle = make_bundle(addr, 2, 200, vec![]);

    let writer = MptDbStateWriter::<reth_ethereum_primitives::Receipt>::new(Arc::clone(&sc));
    writer
        .write_state(
            &ExecutionOutcome::new(bundle, Default::default(), 1, Default::default()),
            OriginalValuesKnown::Yes,
            StateWriteConfig::default(),
        )
        .unwrap();

    let version = sc.lock().version();
    let provider = MptDbStateProvider::new(
        Arc::clone(&sc),
        version,
        MapStateProvider::empty(),
        noop_block_id(),
    );

    use reth_storage_api::StateProofProvider;
    use reth_trie_common::TrieInput;
    let result = provider.proof(TrieInput::default(), addr, &[]);
    assert!(result.is_ok(), "proof() must succeed for committed account, got: {:?}", result.err());
}
