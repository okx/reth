use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, map::HashMap as PrimitivesHashMap, Address, B256, U256};
use mptdb_sc::mpt::{CommitProfile, MptCommitStore, MptCommitter};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use reth_provider::{
    test_utils::{create_test_provider_factory, MockNodeTypesWithDB},
    ProviderFactory, StateWriter, TrieWriter,
};
use reth_trie::{updates::TrieUpdates, HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;
use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
use revm_state::AccountInfo;
use std::{
    collections::HashMap as StdHashMap,
    env,
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub const PREPOP_CHUNK_SIZE: usize = 10_000;
const ERC20_ACTIVE_CONTRACT_POOL_RATIO: f64 = 0.10;
const EOA_INITIAL_BALANCE: u128 = 1_000_000_000_000_000_000;
const ERC20_INITIAL_BALANCE: u128 = 1_000_000;
const ERC20_TRANSFER_AMOUNT: u128 = 100;

// Same runtime bytecode used by mptdb-provider benchmark (393 bytes).
static ERC20_RUNTIME_BYTECODE: [u8; 393] = {
    const HEX: &[u8] = b"608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb1461006a575b5f5ffd5b610057610046366004610104565b5f6020819052908152604090205481565b6040519081526020015b60405180910390f35b61007d610078366004610124565b61008d565b6040519015158152602001610061565b335f908152602081905260408120805483919083906100ad908490610160565b90915550506001600160a01b0383165f90815260208190526040812080548492906100d9908490610173565b9091555060019150505b92915050565b80356001600160a01b03811681146100ff575f5ffd5b919050565b5f60208284031215610114575f5ffd5b61011d826100e9565b9392505050565b5f5f60408385031215610135575f5ffd5b61013e836100e9565b946020939093013593505050565b634e487b7160e01b5f52601160045260245ffd5b818103818111156100e3576100e361014c565b808201808211156100e3576100e361014c56fea26469706673582212202ab3fe360a062c91ad12f573bd1c234812f445f817ce8adbad19c108f60a822d64736f6c634300081e0033";
    const fn h(c: u8) -> u8 {
        if c >= b'0' && c <= b'9' {
            c - b'0'
        } else {
            c - b'a' + 10
        }
    }
    let mut out = [0u8; 393];
    let mut i = 0;
    while i < 393 {
        out[i] = h(HEX[i * 2]) << 4 | h(HEX[i * 2 + 1]);
        i += 1;
    }
    out
};

#[derive(Default)]
pub struct RethBlockProfile {
    pub hash_and_sort: Duration,
    pub root_updates: Duration,
    pub write_hashed: Duration,
    pub write_trie: Duration,
    pub commit: Duration,
    pub total: Duration,
}

#[derive(Default)]
pub struct MptdbTotals {
    pub apply: Duration,
    pub trie_load: Duration,
    pub slot_updates: Duration,
    pub l3_latest: Duration,
    pub l3_published: Duration,
    pub to_tree: Duration,
    pub commit: Duration,
    pub storage_roots: Duration,
    pub storage_roots_prefill: Duration,
    pub storage_roots_take_handles: Duration,
    pub storage_roots_fast_path: Duration,
    pub storage_roots_fast_path_extract: Duration,
    pub storage_roots_fast_path_release: Duration,
    pub storage_roots_fast_path_drop: Duration,
    pub storage_roots_fallback: Duration,
    pub storage_roots_merge: Duration,
    pub account_updates: Duration,
    pub account_root: Duration,
    pub wal_append: Duration,
    pub wal_append_lock_wait: Duration,
    pub wal_append_write: Duration,
    pub wal_serialize: Duration,
    pub wal_crc: Duration,
    pub wal_payload_bytes: u64,
    pub wal_replay: Duration,
    pub durable_materialize: Duration,
    pub published_materialize: Duration,
    pub persist: Duration,
    pub persist_batch: Duration,
    pub manifest_save: Duration,
    pub publish_generation: Duration,
    pub open_published_store: Duration,
    pub cache_publish: Duration,
    pub storage_segment_build: Duration,
    pub storage_root_hashing: Duration,
    pub durable_version_lag: i64,
    pub published_version_lag: i64,
    pub l2_hits: u64,
    pub l3_hits: u64,
    pub acct_trie_checkout: Duration,
    pub ensure_storage: Duration,
    pub storage_root_lookup: Duration,
    pub published_view_refresh: Duration,
    pub commit_acct_set_base: Duration,
    pub commit_cache_prep: Duration,
    pub storage_root_handles: u64,
    pub storage_root_precomputed_handles: u64,
    pub storage_root_rehashed_handles: u64,
}

#[derive(Clone, Copy)]
pub enum ProfileWorkload {
    FreshUniform {
        accounts: usize,
        slots_per: usize,
    },
    Uniform {
        slots_per: usize,
    },
    Mixed {
        contract_slots: usize,
        contract_ratio: f64,
    },
    MixedDecoupled {
        prepop_contract_slots: usize,
        update_contract_slots: usize,
        contract_ratio: f64,
    },
    Erc20PoolLike {
        contract_ratio: f64,
        contract_kv_per_contract: usize,
        active_contract_pool_ratio: f64,
    },
}

#[derive(Clone, Copy)]
pub struct ProfileScenario {
    pub code: &'static str,
    pub title: &'static str,
    pub dataset: &'static str,
    pub addr_seed: u64,
    pub block_seed: u64,
    pub prepop_accounts: usize,
    pub updates_per_block: usize,
    pub block_count: usize,
    pub workload: ProfileWorkload,
}

#[derive(Default)]
pub struct RethRun {
    pub prepop: Duration,
    pub totals: RethBlockProfile,
    pub blocks_len: u32,
}

#[derive(Default)]
pub struct MptRun {
    pub prepop: Duration,
    pub totals: MptdbTotals,
    pub blocks_len: u32,
}

#[derive(Default)]
pub struct BenchRun {
    pub iterations: u32,
    pub prepop_total: Duration,
    pub block_total: Duration,
}

fn wal_first_config() -> mptdb_sc::mpt::MptConfig {
    let mut config = mptdb_sc::mpt::MptConfig::default();
    config.checkpoint_max_account_trie_nodes = 0;
    // BENCHMARK NOTE: use_sparse_storage defaults to true (sparse path).
    // For baseline comparison, set use_sparse_storage=false via env var:
    //   USE_SPARSE=0 cargo test ...
    if std::env::var("USE_SPARSE").as_deref() == Ok("0") {
        config.use_sparse_storage = false;
    }
    if std::env::var("MPT_CROSS_BLOCK_SPARSE").as_deref() == Ok("0") {
        config.cross_block_sparse = false;
    }
    config
}

fn generate_addresses(num: usize, rng: &mut StdRng) -> Vec<Address> {
    let mut addresses = Vec::with_capacity(num);
    let mut addr_buf = [0u8; 20];

    for _ in 0..num {
        rng.fill_bytes(&mut addr_buf);
        addresses.push(Address::from(addr_buf));
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

fn generate_account_chunk_mixed(
    addresses: &[Address],
    global_start: usize,
    total_accounts: usize,
    contract_slots: usize,
    contract_ratio: f64,
) -> revm_database::BundleState {
    let contract_boundary = (total_accounts as f64 * contract_ratio) as usize;
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for (offset, &addr) in addresses.iter().enumerate() {
        let global_idx = global_start + offset;
        let is_contract = global_idx < contract_boundary;
        let slots = if is_contract { contract_slots } else { 0 };
        let info = AccountInfo {
            nonce: global_idx as u64,
            balance: U256::from(1_000_000 * (global_idx + 1)),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        };

        let mut storage = StorageWithOriginalValues::default();
        for j in 0..slots {
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

fn generate_updates_mixed(
    addresses: &[Address],
    count: usize,
    contract_slots: usize,
    contract_ratio: f64,
    block_idx: usize,
    rng: &mut StdRng,
) -> revm_database::BundleState {
    let contract_boundary = (addresses.len() as f64 * contract_ratio) as usize;
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    let indices: Vec<usize> = (0..count).map(|_| rng.random_range(0..addresses.len())).collect();

    for (i, &idx) in indices.iter().enumerate() {
        let addr = addresses[idx];
        let is_contract = idx < contract_boundary;
        let nonce = (block_idx * count + i) as u64;
        let balance = U256::from(1_000_000 * (block_idx * count + i + 1));
        let info =
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, account_id: None, code: None };

        let mut storage = StorageWithOriginalValues::default();
        if is_contract {
            for j in 0..contract_slots {
                let mut slot_bytes = [0u8; 32];
                slot_bytes[24..32].copy_from_slice(&(j as u64).to_be_bytes());
                storage.insert(
                    B256::from(slot_bytes).into(),
                    StorageSlot::new_changed(U256::ZERO, U256::from((block_idx + j) as u128 + 1)),
                );
            }
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

#[derive(Clone, Copy)]
struct PoolLayout {
    contract_count: usize,
    eoa_offset: usize,
    eoa_count: usize,
    kv_per_contract: usize,
    active_contracts: usize,
}

fn erc20_runtime_code_hash() -> B256 {
    keccak256(ERC20_RUNTIME_BYTECODE)
}

fn erc20_balance_slot(holder: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    keccak256(buf)
}

fn build_pool_layout(
    total_accounts: usize,
    updates_per_block: usize,
    contract_ratio: f64,
    contract_kv_per_contract: usize,
    active_contract_pool_ratio: f64,
) -> PoolLayout {
    let min_eoa = updates_per_block.max(1).min(total_accounts);
    let requested_contracts = ((total_accounts as f64) * contract_ratio).round() as usize;
    let max_contracts = total_accounts.saturating_sub(min_eoa);
    let contract_count = requested_contracts.min(max_contracts);
    let eoa_offset = contract_count;
    let eoa_count = total_accounts.saturating_sub(contract_count);
    let kv_per_contract = contract_kv_per_contract.min(eoa_count.max(1));
    let active_contracts = if contract_count == 0 {
        0
    } else {
        ((contract_count as f64) * active_contract_pool_ratio)
            .ceil()
            .max(1.0)
            .min(contract_count as f64) as usize
    };
    PoolLayout { contract_count, eoa_offset, eoa_count, kv_per_contract, active_contracts }
}

fn is_initial_holder(layout: PoolLayout, contract_idx: usize, eoa_local_idx: usize) -> bool {
    if layout.eoa_count == 0 || layout.kv_per_contract == 0 {
        return false;
    }
    if layout.kv_per_contract >= layout.eoa_count {
        return true;
    }
    let start = (contract_idx * layout.kv_per_contract) % layout.eoa_count;
    let end = start + layout.kv_per_contract;
    if end <= layout.eoa_count {
        eoa_local_idx >= start && eoa_local_idx < end
    } else {
        eoa_local_idx >= start || eoa_local_idx < (end - layout.eoa_count)
    }
}

fn initial_erc20_balance_for_holder(
    layout: PoolLayout,
    contract_idx: usize,
    eoa_local_idx: usize,
) -> u128 {
    if is_initial_holder(layout, contract_idx, eoa_local_idx) {
        ERC20_INITIAL_BALANCE
    } else {
        0
    }
}

fn pair_key(contract_idx: usize, eoa_local_idx: usize) -> u64 {
    ((contract_idx as u64) << 32) | (eoa_local_idx as u64)
}

fn split_pair_key(key: u64) -> (usize, usize) {
    ((key >> 32) as usize, (key & 0xffff_ffff) as usize)
}

fn generate_account_chunk_erc20_pool_like(
    all_addresses: &[Address],
    global_start: usize,
    global_end: usize,
    layout: PoolLayout,
) -> revm_database::BundleState {
    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    let contract_code_hash = erc20_runtime_code_hash();

    for global_idx in global_start..global_end {
        let addr = all_addresses[global_idx];
        if global_idx < layout.contract_count {
            let mut storage = StorageWithOriginalValues::default();
            for j in 0..layout.kv_per_contract {
                let holder_local_idx = (global_idx * layout.kv_per_contract + j) % layout.eoa_count;
                let holder = all_addresses[layout.eoa_offset + holder_local_idx];
                storage.insert(
                    erc20_balance_slot(holder).into(),
                    StorageSlot::new_changed(U256::ZERO, U256::from(ERC20_INITIAL_BALANCE)),
                );
            }
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(AccountInfo {
                        nonce: 1,
                        balance: U256::ZERO,
                        code_hash: contract_code_hash,
                        account_id: None,
                        code: None,
                    }),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage,
                },
            );
        } else {
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(AccountInfo {
                        nonce: 0,
                        balance: U256::from(EOA_INITIAL_BALANCE),
                        code_hash: KECCAK_EMPTY,
                        account_id: None,
                        code: None,
                    }),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage: StorageWithOriginalValues::default(),
                },
            );
        }
    }

    revm_database::BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
}

fn apply_pool_transfer_delta(
    layout: PoolLayout,
    current_balances: &mut StdHashMap<u64, u128>,
    block_changes: &mut StdHashMap<u64, (u128, u128)>,
    contract_idx: usize,
    eoa_local_idx: usize,
    is_credit: bool,
) {
    let key = pair_key(contract_idx, eoa_local_idx);
    let prev = current_balances
        .get(&key)
        .copied()
        .unwrap_or_else(|| initial_erc20_balance_for_holder(layout, contract_idx, eoa_local_idx));
    let new = if is_credit {
        prev.saturating_add(ERC20_TRANSFER_AMOUNT)
    } else {
        prev.saturating_sub(ERC20_TRANSFER_AMOUNT)
    };
    current_balances.insert(key, new);
    block_changes.entry(key).and_modify(|entry| entry.1 = new).or_insert((prev, new));
}

fn generate_updates_erc20_pool_like(
    addresses: &[Address],
    updates_per_block: usize,
    block_count: usize,
    layout: PoolLayout,
    rng: &mut StdRng,
) -> Vec<revm_database::BundleState> {
    let mut blocks = Vec::with_capacity(block_count);
    let contract_code_hash = erc20_runtime_code_hash();
    let mut sender_nonces = vec![0u64; layout.eoa_count];
    let mut current_balances: StdHashMap<u64, u128> = StdHashMap::default();

    for _ in 0..block_count {
        let mut sender_nonce_updates: StdHashMap<usize, u64> = StdHashMap::default();
        let mut block_storage_changes: StdHashMap<u64, (u128, u128)> = StdHashMap::default();

        for _ in 0..updates_per_block {
            let contract_idx = rng.random_range(0..layout.active_contracts);
            let sender_local = (contract_idx * layout.kv_per_contract +
                rng.random_range(0..layout.kv_per_contract)) %
                layout.eoa_count;
            let receiver_local = rng.random_range(0..layout.eoa_count);

            sender_nonces[sender_local] = sender_nonces[sender_local].saturating_add(1);
            sender_nonce_updates.insert(sender_local, sender_nonces[sender_local]);

            apply_pool_transfer_delta(
                layout,
                &mut current_balances,
                &mut block_storage_changes,
                contract_idx,
                sender_local,
                false,
            );
            apply_pool_transfer_delta(
                layout,
                &mut current_balances,
                &mut block_storage_changes,
                contract_idx,
                receiver_local,
                true,
            );
        }

        let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
            PrimitivesHashMap::default();
        for (eoa_local_idx, nonce) in sender_nonce_updates {
            let addr = addresses[layout.eoa_offset + eoa_local_idx];
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(AccountInfo {
                        nonce,
                        balance: U256::from(EOA_INITIAL_BALANCE),
                        code_hash: KECCAK_EMPTY,
                        account_id: None,
                        code: None,
                    }),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage: StorageWithOriginalValues::default(),
                },
            );
        }

        let mut ordered_changes: Vec<(u64, (u128, u128))> =
            block_storage_changes.into_iter().collect();
        ordered_changes.sort_unstable_by_key(|(key, _)| *key);
        for (key, (old, new)) in ordered_changes {
            if old == new {
                continue;
            }
            let (contract_idx, eoa_local_idx) = split_pair_key(key);
            let contract = addresses[contract_idx];
            let holder = addresses[layout.eoa_offset + eoa_local_idx];
            let account = state.entry(contract).or_insert_with(|| revm_database::BundleAccount {
                info: Some(AccountInfo {
                    nonce: 1,
                    balance: U256::ZERO,
                    code_hash: contract_code_hash,
                    account_id: None,
                    code: None,
                }),
                original_info: None,
                status: AccountStatus::Changed,
                storage: StorageWithOriginalValues::default(),
            });
            account.storage.insert(
                erc20_balance_slot(holder).into(),
                StorageSlot::new_changed(U256::from(old), U256::from(new)),
            );
        }

        blocks.push(revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        });
    }

    blocks
}

fn accumulate_mptdb_profile(totals: &mut MptdbTotals, profile: &CommitProfile) {
    totals.trie_load += profile.apply_get_or_load_storage_tries;
    totals.slot_updates += profile.apply_storage_slot_updates;
    totals.l3_latest += profile.apply_l3_latest_load;
    totals.l3_published += profile.apply_l3_published_load;
    totals.to_tree += profile.apply_l3_into_tree;
    totals.commit += profile.total_commit;
    totals.storage_roots += profile.storage_roots;
    totals.storage_roots_prefill += profile.storage_roots_prefill;
    totals.storage_roots_take_handles += profile.storage_roots_take_handles;
    totals.storage_roots_fast_path += profile.storage_roots_fast_path_collect;
    totals.storage_roots_fast_path_extract += profile.storage_roots_fast_path_extract;
    totals.storage_roots_fast_path_release += profile.storage_roots_fast_path_release;
    totals.storage_roots_fast_path_drop += profile.storage_roots_fast_path_drop;
    totals.storage_roots_fallback += profile.storage_roots_fallback_collect;
    totals.storage_roots_merge += profile.storage_roots_merge;
    totals.account_updates += profile.account_updates;
    totals.account_root += profile.account_root_and_blobs;
    totals.wal_append += profile.wal_append;
    totals.wal_append_lock_wait += profile.wal_append_lock_wait;
    totals.wal_append_write += profile.wal_append_write;
    totals.wal_serialize += profile.wal_serialize;
    totals.wal_crc += profile.wal_crc;
    totals.wal_payload_bytes += profile.wal_payload_bytes as u64;
    totals.wal_replay += profile.wal_replay;
    totals.durable_materialize += profile.durable_materialize;
    totals.published_materialize += profile.published_materialize;
    totals.persist += profile.persist_and_manifest;
    totals.persist_batch += profile.persist_batch;
    totals.manifest_save += profile.manifest_save;
    totals.publish_generation += profile.publish_generation;
    totals.open_published_store += profile.open_published_store;
    totals.cache_publish += profile.cache_publish;
    totals.storage_segment_build += profile.storage_segment_build;
    totals.storage_root_hashing += profile.storage_root_hashing;
    totals.durable_version_lag += profile.durable_version_lag;
    totals.published_version_lag += profile.published_version_lag;
    totals.l2_hits += profile.apply_l2_hits;
    totals.l3_hits += profile.apply_l3_latest_hits +
        profile.apply_l3_published_hits +
        profile.apply_l3_published_post_flush_hits;
    totals.acct_trie_checkout += profile.apply_account_trie_checkout;
    totals.ensure_storage += profile.apply_ensure_storage;
    totals.storage_root_lookup += profile.apply_storage_root_lookup;
    totals.published_view_refresh += profile.apply_published_view_refresh;
    totals.commit_acct_set_base += profile.commit_account_set_base;
    totals.commit_cache_prep += profile.commit_cache_storage_prep;
    totals.storage_root_handles += profile.storage_roots_working_handles;
    totals.storage_root_precomputed_handles += profile.storage_roots_precomputed_handles;
    totals.storage_root_rehashed_handles += profile.storage_roots_rehashed_handles;
}

pub fn fmt_ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

fn profile_trace_enabled() -> bool {
    env::var_os("MPT_PROFILE_TRACE").is_some()
}

pub fn scenario_b4_1() -> ProfileScenario {
    ProfileScenario {
        code: "B4.1",
        title: "Fresh-State One-Shot",
        dataset: "fresh state, 1 block x 100 accounts x 10 slots",
        addr_seed: 4100,
        block_seed: 4101,
        prepop_accounts: 0,
        updates_per_block: 100,
        block_count: 1,
        workload: ProfileWorkload::FreshUniform { accounts: 100, slots_per: 10 },
    }
}

pub fn scenario_b4_2() -> ProfileScenario {
    ProfileScenario {
        code: "B4.2",
        title: "Prepop + Single Block",
        dataset: "1K pre-pop x 10 slots, 1 block x 200 updates",
        addr_seed: 4200,
        block_seed: 4201,
        prepop_accounts: 1_000,
        updates_per_block: 200,
        block_count: 1,
        workload: ProfileWorkload::Uniform { slots_per: 10 },
    }
}

pub fn scenario_b4_3() -> ProfileScenario {
    ProfileScenario {
        code: "B4.3",
        title: "Incremental 10 Blocks",
        dataset: "1K pre-pop x 10 slots, 10 blocks x 200 updates",
        addr_seed: 4300,
        block_seed: 4301,
        prepop_accounts: 1_000,
        updates_per_block: 200,
        block_count: 10,
        workload: ProfileWorkload::Uniform { slots_per: 10 },
    }
}

pub fn scenario_b4_4() -> ProfileScenario {
    ProfileScenario {
        code: "B4.4",
        title: "Single-Run Compare",
        dataset: "200K pre-pop x 10 slots, 10 blocks x 2K updates",
        addr_seed: 4400,
        block_seed: 4401,
        prepop_accounts: 200_000,
        updates_per_block: 2_000,
        block_count: 10,
        workload: ProfileWorkload::Uniform { slots_per: 10 },
    }
}

pub fn scenario_b4_5() -> ProfileScenario {
    ProfileScenario {
        code: "B4.5",
        title: "Single-Run Compare",
        dataset: "1M pre-pop x 10 slots, 10 blocks x 5K updates",
        addr_seed: 4500,
        block_seed: 4501,
        prepop_accounts: 1_000_000,
        updates_per_block: 5_000,
        block_count: 10,
        workload: ProfileWorkload::Uniform { slots_per: 10 },
    }
}

pub fn scenario_b4_6() -> ProfileScenario {
    ProfileScenario {
        code: "B4.6",
        title: "Single-Run Compare",
        dataset: "1M pre-pop x 30 slots, 10 blocks x 10K updates",
        addr_seed: 4600,
        block_seed: 4601,
        prepop_accounts: 1_000_000,
        updates_per_block: 10_000,
        block_count: 10,
        workload: ProfileWorkload::Uniform { slots_per: 30 },
    }
}

pub fn scenario_b4_7() -> ProfileScenario {
    ProfileScenario {
        code: "B4.7",
        title: "Mainnet-Realistic",
        dataset: "500K accounts, 30% contracts x 200 slots, 10 blocks x 1K updates",
        addr_seed: 4700,
        block_seed: 4701,
        prepop_accounts: 500_000,
        updates_per_block: 1_000,
        block_count: 10,
        workload: ProfileWorkload::Mixed { contract_slots: 200, contract_ratio: 0.3 },
    }
}

pub fn scenario_b4_8() -> ProfileScenario {
    ProfileScenario {
        code: "B4.8",
        title: "Provider-Equivalent ERC20 Pool",
        dataset:
            "500K accounts, 30% contracts x 128 pre-pop holders, active pool 10%, 10 blocks x 50K tx-style updates",
        addr_seed: 4800,
        block_seed: 4801,
        prepop_accounts: 500_000,
        updates_per_block: 50_000,
        block_count: 10,
        workload: ProfileWorkload::Erc20PoolLike {
            contract_ratio: 0.3,
            contract_kv_per_contract: 128,
            active_contract_pool_ratio: ERC20_ACTIVE_CONTRACT_POOL_RATIO,
        },
    }
}

pub fn generate_profile_inputs(
    scenario: ProfileScenario,
) -> (Vec<Address>, Vec<revm_database::BundleState>) {
    let address_count = match scenario.workload {
        ProfileWorkload::FreshUniform { accounts, .. } => accounts,
        _ => scenario.prepop_accounts,
    };

    let mut rng = StdRng::seed_from_u64(scenario.addr_seed);
    let addrs = generate_addresses(address_count, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(scenario.block_seed);
    let blocks = match scenario.workload {
        ProfileWorkload::Erc20PoolLike {
            contract_ratio,
            contract_kv_per_contract,
            active_contract_pool_ratio,
        } => {
            let layout = build_pool_layout(
                addrs.len(),
                scenario.updates_per_block,
                contract_ratio,
                contract_kv_per_contract,
                active_contract_pool_ratio,
            );
            generate_updates_erc20_pool_like(
                &addrs,
                scenario.updates_per_block,
                scenario.block_count,
                layout,
                &mut rng_blocks,
            )
        }
        _ => (0..scenario.block_count)
            .map(|i| match scenario.workload {
                ProfileWorkload::FreshUniform { slots_per, .. } => {
                    generate_account_chunk(&addrs, 0, slots_per)
                }
                ProfileWorkload::Uniform { slots_per } => generate_updates(
                    &addrs,
                    scenario.updates_per_block,
                    slots_per,
                    i,
                    &mut rng_blocks,
                ),
                ProfileWorkload::Mixed { contract_slots, contract_ratio } => {
                    generate_updates_mixed(
                        &addrs,
                        scenario.updates_per_block,
                        contract_slots,
                        contract_ratio,
                        i,
                        &mut rng_blocks,
                    )
                }
                ProfileWorkload::MixedDecoupled {
                    update_contract_slots, contract_ratio, ..
                } => generate_updates_mixed(
                    &addrs,
                    scenario.updates_per_block,
                    update_contract_slots,
                    contract_ratio,
                    i,
                    &mut rng_blocks,
                ),
                ProfileWorkload::Erc20PoolLike { .. } => unreachable!(),
            })
            .collect(),
    };

    (addrs, blocks)
}

pub fn prepopulate_reth_profile(
    factory: &ProviderFactory<MockNodeTypesWithDB>,
    scenario: ProfileScenario,
    addrs: &[Address],
) -> Duration {
    let start = Instant::now();
    let total = addrs.len();
    for (chunk_idx, chunk) in addrs.chunks(PREPOP_CHUNK_SIZE).enumerate() {
        let global_start = chunk_idx * PREPOP_CHUNK_SIZE;
        let bundle = match scenario.workload {
            ProfileWorkload::FreshUniform { .. } => {
                unreachable!("fresh-state scenario has no pre-population")
            }
            ProfileWorkload::Uniform { slots_per } => {
                generate_account_chunk(chunk, global_start, slots_per)
            }
            ProfileWorkload::Mixed { contract_slots, contract_ratio } => {
                generate_account_chunk_mixed(
                    chunk,
                    global_start,
                    total,
                    contract_slots,
                    contract_ratio,
                )
            }
            ProfileWorkload::MixedDecoupled { prepop_contract_slots, contract_ratio, .. } => {
                generate_account_chunk_mixed(
                    chunk,
                    global_start,
                    total,
                    prepop_contract_slots,
                    contract_ratio,
                )
            }
            ProfileWorkload::Erc20PoolLike {
                contract_ratio,
                contract_kv_per_contract,
                active_contract_pool_ratio,
            } => {
                let layout = build_pool_layout(
                    total,
                    scenario.updates_per_block,
                    contract_ratio,
                    contract_kv_per_contract,
                    active_contract_pool_ratio,
                );
                generate_account_chunk_erc20_pool_like(
                    addrs,
                    global_start,
                    global_start + chunk.len(),
                    layout,
                )
            }
        };
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
}

pub fn prepopulate_mpt_profile(
    store: &mut MptCommitStore,
    scenario: ProfileScenario,
    addrs: &[Address],
) -> Duration {
    let start = Instant::now();
    let total = addrs.len();
    for (chunk_idx, chunk) in addrs.chunks(PREPOP_CHUNK_SIZE).enumerate() {
        let global_start = chunk_idx * PREPOP_CHUNK_SIZE;
        let bundle = match scenario.workload {
            ProfileWorkload::FreshUniform { .. } => {
                unreachable!("fresh-state scenario has no pre-population")
            }
            ProfileWorkload::Uniform { slots_per } => {
                generate_account_chunk(chunk, global_start, slots_per)
            }
            ProfileWorkload::Mixed { contract_slots, contract_ratio } => {
                generate_account_chunk_mixed(
                    chunk,
                    global_start,
                    total,
                    contract_slots,
                    contract_ratio,
                )
            }
            ProfileWorkload::MixedDecoupled { prepop_contract_slots, contract_ratio, .. } => {
                generate_account_chunk_mixed(
                    chunk,
                    global_start,
                    total,
                    prepop_contract_slots,
                    contract_ratio,
                )
            }
            ProfileWorkload::Erc20PoolLike {
                contract_ratio,
                contract_kv_per_contract,
                active_contract_pool_ratio,
            } => {
                let layout = build_pool_layout(
                    total,
                    scenario.updates_per_block,
                    contract_ratio,
                    contract_kv_per_contract,
                    active_contract_pool_ratio,
                );
                generate_account_chunk_erc20_pool_like(
                    addrs,
                    global_start,
                    global_start + chunk.len(),
                    layout,
                )
            }
        };
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
    }
    start.elapsed()
}

pub fn print_profile_header(scenario: ProfileScenario, suffix: &str, prepop_accounts: usize) {
    println!("\n=== {} {} ===", scenario.code, suffix);
    println!("Dataset: {}", scenario.dataset);
    println!(
        "Pre-pop mode: {} chunks x {} accounts",
        prepop_accounts.div_ceil(PREPOP_CHUNK_SIZE),
        PREPOP_CHUNK_SIZE
    );
}

pub fn run_reth_only_profile(scenario: ProfileScenario) -> RethRun {
    let (addrs, blocks) = generate_profile_inputs(scenario);
    let factory = create_test_provider_factory();
    let reth_prepop = if scenario.prepop_accounts == 0 {
        Duration::ZERO
    } else {
        prepopulate_reth_profile(&factory, scenario, &addrs)
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

    RethRun { prepop: reth_prepop, totals: reth_totals, blocks_len: blocks.len() as u32 }
}

pub fn run_mpt_only_profile(scenario: ProfileScenario) -> MptRun {
    let (addrs, blocks) = generate_profile_inputs(scenario);
    let dir = TempDir::new().unwrap();
    let mut store =
        MptCommitStore::open_with_config(dir.path(), false, wal_first_config()).unwrap();
    let trace_enabled = profile_trace_enabled();

    let mptdb_prepop = if scenario.prepop_accounts == 0 {
        Duration::ZERO
    } else {
        prepopulate_mpt_profile(&mut store, scenario, &addrs)
    };
    // Wait for the background persist worker to finish publishing all pre-pop
    // segments before starting timed profile blocks.  Without this, early
    // profile blocks may miss L3 segment lookups (worker not yet done) and
    // fall back to persisted store, causing run-to-run timing instability.
    if scenario.prepop_accounts > 0 {
        store.flush_persist().unwrap();
    }
    let enable_block_prewarm = matches!(scenario.workload, ProfileWorkload::Erc20PoolLike { .. });
    let mut mptdb_totals = MptdbTotals::default();
    for (block_idx, block) in blocks.iter().enumerate() {
        let apply_start = Instant::now();
        store.apply_bundle_state(block).unwrap();
        mptdb_totals.apply += apply_start.elapsed();

        let ((_version, _root), profile): ((i64, B256), CommitProfile) =
            store.commit_with_profile().unwrap();
        if trace_enabled {
            let l3_hits = profile.apply_l3_latest_hits +
                profile.apply_l3_published_hits +
                profile.apply_l3_published_post_flush_hits;
            println!(
                "  block {:>2}: total={} apply={} commit={} trie_load={} slot_updates={} \
storage_roots={} wal_append={} wal_lock_wait={} l3_load={} l3_hits={} root_handles={}/{} \
sseg={}/{} smiss={} srm={} t3={}/{} t12={} creuse={} cmiss={} \
sp_fb={} sp_ap={} sp_ch={}",
                block_idx + 1,
                fmt_ms(profile.apply_bundle_state + profile.total_commit),
                fmt_ms(profile.apply_bundle_state),
                fmt_ms(profile.total_commit),
                fmt_ms(profile.apply_get_or_load_storage_tries),
                fmt_ms(profile.apply_storage_slot_updates),
                fmt_ms(profile.storage_roots),
                fmt_ms(profile.wal_append),
                fmt_ms(profile.wal_append_lock_wait),
                fmt_ms(profile.apply_l3_published_load),
                l3_hits,
                profile.storage_roots_precomputed_handles,
                profile.storage_roots_working_handles,
                profile.sparse_factory_segment_hits,
                profile.sparse_factory_segment_lookups,
                profile.sparse_factory_segment_miss + profile.sparse_factory_segment_miss_no_store,
                profile.sparse_factory_segment_root_mismatch,
                profile.sparse_factory_tier3_hits,
                profile.sparse_factory_tier3_attempts,
                profile.sparse_factory_tier12_attempts,
                profile.sparse_factory_cross_reuse_accounts,
                profile.sparse_factory_cross_missing_slots,
                fmt_ms(profile.sparse_apply_factory_build),
                fmt_ms(profile.sparse_apply_account_proof),
                fmt_ms(profile.sparse_apply_apply_changes),
            );
        }
        accumulate_mptdb_profile(&mut mptdb_totals, &profile);

        if enable_block_prewarm {
            store.maybe_refresh_published_view_for_prewarm();
            for (addr, account) in block.state().iter() {
                if !account.storage.is_empty() {
                    let _ =
                        store.prewarm_storage_trie_by_hashed_address(keccak256(addr.as_slice()));
                }
            }
            // For provider-equivalent heavy pool workloads, force publish before
            // the next block so storage-root lookups do not depend on async lag.
            store.flush_persist().unwrap();
        }
    }
    store.close().unwrap();

    MptRun { prepop: mptdb_prepop, totals: mptdb_totals, blocks_len: blocks.len() as u32 }
}

pub fn print_reth_run(run: &RethRun) {
    let blocks = run.blocks_len;
    let per_block_total = run.totals.total / blocks;
    let per_block_root_plus_commit = (run.totals.root_updates + run.totals.commit) / blocks;
    let per_block_root_trie_commit =
        (run.totals.root_updates + run.totals.write_trie + run.totals.commit) / blocks;
    let per_block_other_vs_root_commit = per_block_total.saturating_sub(per_block_root_plus_commit);
    let per_block_other_vs_root_trie_commit =
        per_block_total.saturating_sub(per_block_root_trie_commit);

    println!("\nreth");
    println!("  pre-pop total:       {} ms", fmt_ms(run.prepop));
    println!("  per-block total:     {} ms", fmt_ms(per_block_total));
    println!("  hash+sort:           {} ms", fmt_ms(run.totals.hash_and_sort / blocks));
    println!("  root_with_updates:   {} ms", fmt_ms(run.totals.root_updates / blocks));
    println!("  write_hashed_state:  {} ms", fmt_ms(run.totals.write_hashed / blocks));
    println!("  write_trie_updates:  {} ms", fmt_ms(run.totals.write_trie / blocks));
    println!("  commit:              {} ms", fmt_ms(run.totals.commit / blocks));
    println!("  root+commit:         {} ms", fmt_ms(per_block_root_plus_commit));
    println!("  root+trie+commit:    {} ms", fmt_ms(per_block_root_trie_commit));
    println!("  other(total-root+commit):      {} ms", fmt_ms(per_block_other_vs_root_commit));
    println!("  other(total-root+trie+commit): {} ms", fmt_ms(per_block_other_vs_root_trie_commit));
}

pub fn print_mpt_run(run: &MptRun) {
    println!("\nmpt-db");
    println!("  pre-pop total:       {} ms", fmt_ms(run.prepop));
    println!(
        "  per-block total:     {} ms",
        fmt_ms((run.totals.apply + run.totals.commit) / run.blocks_len)
    );
    println!("  apply_bundle_state:  {} ms", fmt_ms(run.totals.apply / run.blocks_len));
    println!("  trie_load:           {} ms", fmt_ms(run.totals.trie_load / run.blocks_len));
    println!(
        "    account_checkout:  {} ms",
        fmt_ms(run.totals.acct_trie_checkout / run.blocks_len)
    );
    println!("    ensure_storage:    {} ms", fmt_ms(run.totals.ensure_storage / run.blocks_len));
    println!(
        "      root_lookup:     {} ms",
        fmt_ms(run.totals.storage_root_lookup / run.blocks_len)
    );
    println!(
        "      view_refresh:    {} ms",
        fmt_ms(run.totals.published_view_refresh / run.blocks_len)
    );
    println!("      l3_published:    {} ms", fmt_ms(run.totals.l3_published / run.blocks_len));
    println!("  slot_updates:        {} ms", fmt_ms(run.totals.slot_updates / run.blocks_len));
    println!("  commit:              {} ms", fmt_ms(run.totals.commit / run.blocks_len));
    println!("  storage_roots:       {} ms", fmt_ms(run.totals.storage_roots / run.blocks_len));
    println!(
        "    prefill:           {} ms",
        fmt_ms(run.totals.storage_roots_prefill / run.blocks_len)
    );
    println!(
        "    take_handles:      {} ms",
        fmt_ms(run.totals.storage_roots_take_handles / run.blocks_len)
    );
    println!(
        "    fast_path_collect: {} ms",
        fmt_ms(run.totals.storage_roots_fast_path / run.blocks_len)
    );
    println!(
        "      extract:         {} ms",
        fmt_ms(run.totals.storage_roots_fast_path_extract / run.blocks_len)
    );
    println!(
        "      release:         {} ms",
        fmt_ms(run.totals.storage_roots_fast_path_release / run.blocks_len)
    );
    println!(
        "    fast_path_drop:    {} ms",
        fmt_ms(run.totals.storage_roots_fast_path_drop / run.blocks_len)
    );
    println!(
        "    fallback_collect:  {} ms",
        fmt_ms(run.totals.storage_roots_fallback / run.blocks_len)
    );
    println!(
        "    merge:             {} ms",
        fmt_ms(run.totals.storage_roots_merge / run.blocks_len)
    );
    println!("  account_updates:     {} ms", fmt_ms(run.totals.account_updates / run.blocks_len));
    println!("  account_root:        {} ms", fmt_ms(run.totals.account_root / run.blocks_len));
    println!("  wal_append:          {} ms", fmt_ms(run.totals.wal_append / run.blocks_len));
    println!(
        "    wal_lock_wait:     {} ms",
        fmt_ms(run.totals.wal_append_lock_wait / run.blocks_len)
    );
    println!("    wal_write:         {} ms", fmt_ms(run.totals.wal_append_write / run.blocks_len));
    println!(
        "      serialize:       {} ms ({} KB)",
        fmt_ms(run.totals.wal_serialize / run.blocks_len),
        run.totals.wal_payload_bytes / run.blocks_len as u64 / 1024
    );
    println!("      crc:             {} ms", fmt_ms(run.totals.wal_crc / run.blocks_len));
    println!("  persist:             {} ms", fmt_ms(run.totals.persist / run.blocks_len));
    println!("  cache_publish:       {} ms", fmt_ms(run.totals.cache_publish / run.blocks_len));
    println!("  cache_prep:          {} ms", fmt_ms(run.totals.commit_cache_prep / run.blocks_len));
    println!(
        "  segment_build:       {} ms",
        fmt_ms(run.totals.storage_segment_build / run.blocks_len)
    );
    println!(
        "  root_hashing:        {} ms",
        fmt_ms(run.totals.storage_root_hashing / run.blocks_len)
    );
    println!(
        "  avg hits/block:      L2={}, L3={}",
        run.totals.l2_hits / run.blocks_len as u64,
        run.totals.l3_hits / run.blocks_len as u64
    );
    println!(
        "  avg root handles:    total={}, precomputed={}, rehashed={}",
        run.totals.storage_root_handles / run.blocks_len as u64,
        run.totals.storage_root_precomputed_handles / run.blocks_len as u64,
        run.totals.storage_root_rehashed_handles / run.blocks_len as u64
    );
}

pub fn run_profile_compare(scenario: ProfileScenario) {
    print_profile_header(scenario, scenario.title, scenario.prepop_accounts);
    let reth = run_reth_only_profile(scenario);
    let mpt = run_mpt_only_profile(scenario);
    print_reth_run(&reth);
    print_mpt_run(&mpt);

    let reth_avg = reth.totals.total / reth.blocks_len;
    let mpt_avg = (mpt.totals.apply + mpt.totals.commit) / mpt.blocks_len;
    println!(
        "\napprox ratio (single run): mpt-db / reth = {:.2}x",
        mpt_avg.as_secs_f64() / reth_avg.as_secs_f64()
    );
}

pub fn run_profile_reth_only(scenario: ProfileScenario) {
    print_profile_header(scenario, "reth Only", scenario.prepop_accounts);
    let reth = run_reth_only_profile(scenario);
    print_reth_run(&reth);
}

pub fn run_profile_mpt_only(scenario: ProfileScenario) {
    print_profile_header(scenario, "mpt-db Only", scenario.prepop_accounts);
    let mpt = run_mpt_only_profile(scenario);
    print_mpt_run(&mpt);
}

pub fn benchmark_iterations_from_env() -> u32 {
    env::var("MPT_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|iters| *iters > 0)
        .unwrap_or(10)
}

pub fn run_reth_benchmark(scenario: ProfileScenario, iterations: u32) -> BenchRun {
    let mut summary = BenchRun { iterations, ..BenchRun::default() };
    for _ in 0..iterations {
        let run = run_reth_only_profile(scenario);
        summary.prepop_total += run.prepop;
        summary.block_total += run.totals.total / run.blocks_len;
    }
    summary
}

pub fn run_mpt_benchmark(scenario: ProfileScenario, iterations: u32) -> BenchRun {
    let mut summary = BenchRun { iterations, ..BenchRun::default() };
    for _ in 0..iterations {
        let run = run_mpt_only_profile(scenario);
        summary.prepop_total += run.prepop;
        summary.block_total += (run.totals.apply + run.totals.commit) / run.blocks_len;
    }
    summary
}

pub fn print_reth_benchmark(scenario: ProfileScenario, summary: &BenchRun) {
    print_profile_header(scenario, "reth Benchmark", scenario.prepop_accounts);
    println!("samples:               {}", summary.iterations);
    println!("avg pre-pop total:     {} ms", fmt_ms(summary.prepop_total / summary.iterations));
    println!("avg per-block total:   {} ms", fmt_ms(summary.block_total / summary.iterations));
}

pub fn print_mpt_benchmark(scenario: ProfileScenario, summary: &BenchRun) {
    print_profile_header(scenario, "mpt-db Benchmark", scenario.prepop_accounts);
    println!("samples:               {}", summary.iterations);
    println!("avg pre-pop total:     {} ms", fmt_ms(summary.prepop_total / summary.iterations));
    println!("avg per-block total:   {} ms", fmt_ms(summary.block_total / summary.iterations));
}
