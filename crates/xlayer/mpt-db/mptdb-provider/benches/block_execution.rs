//! Block execution benchmark: mptdb SC (state root) vs reth native MDBX.
//!
//! ## What this benchmark measures
//!
//! For each block:
//! - **mptdb lane**: default EVM reads from MDBX directly; set
//!   `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1` to route reads through `MptDbStateProvider`. SC
//!   commit (apply + WAL + state root) and MDBX PlainState write run in parallel. Set By default
//!   this benchmark enables async plain materialization (wal-first style), so block hot-path does
//!   not wait MDBX plain writes per block. Set `MPTDB_PROVIDER_BENCH_ASYNC_PLAIN_MATERIALIZATION=0`
//!   to disable for A/B comparison.
//! - **reth-mdbx lane**: EVM reads from MDBX, MDBX write + overlay_root_with_updates
//!   + write_trie_updates + commit (full reth persistence path).
//!
//! ## Known benchmark design choices
//!
//! 1. **MptDbStateProvider is NOT in the mptdb EVM read path here.** EVM reads use
//!    `mdbx_factory.latest()` directly (no Mutex, no provider wrapper). In production, reads go
//!    through `SyncProvider(Mutex<StateProviderBox>)` injected by StateProviderOverride, adding a
//!    per-read lock overhead not measured here. This benchmark therefore **understates** the real
//!    mptdb EVM read overhead. It measures SC write (state root computation) vs reth MDBX
//!    write+root, not the full provider-layer read cost.
//!
//! 2. **mptdb lane skips trie updates, but still writes MDBX state tables.** `write_state` writes
//!    PlainState and related change/history tables; we intentionally skip
//!    `overlay_root_with_updates + write_trie_updates`, so MDBX trie tables
//!    (AccountsTrie/StoragesTrie) are left empty. This matches the Plan C target where SC owns
//!    state root computation while MDBX serves EVM reads.
//!
//! Run:
//!   cargo bench --bench block_execution -p mptdb-provider
//!   cargo bench --bench block_execution -p mptdb-provider -- "eth_transfer"

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use mptdb_provider::{MptDbStateProvider, MptDbStateWriter, ScPrewarmDispatcher, SyncProvider};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter, MptConfig};
use parking_lot::Mutex;
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::prelude::*;
use reth_ethereum_primitives::Receipt as EthReceipt;
use reth_storage_api::{
    errors::provider::ProviderError, BlockIdReader, StateProvider, StateWriteConfig,
};
use reth_trie_common::HashedPostState;
use revm::{
    context::{BlockEnv, Context, TxEnv},
    database::states::bundle_state::BundleRetention,
    handler::MainnetContext,
    primitives::hardfork::SpecId,
    ExecuteCommitEvm, MainBuilder,
};
use revm_database::{BundleState, OriginalValuesKnown, State};
use revm_state::AccountInfo;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;

// ── Config ────────────────────────────────────────────────────────────────────
const DEFAULT_PRE_POP_ACCOUNTS: usize = 100_000;
const DEFAULT_NUM_BLOCKS: usize = 10;
const DEFAULT_TXS_PER_BLOCK: usize = 20_000;
const INITIAL_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH
const DEFAULT_SAMPLE_SIZE: usize = 10;
const DEFAULT_WARMUP_SECS: u64 = 3;
const DEFAULT_MEASUREMENT_SECS: u64 = 120;
const DEFAULT_CONTRACT_RATIO: f64 = 0.30;
const DEFAULT_CONTRACT_KV_PER_CONTRACT: usize = 32;
const ERC20_ACTIVE_CONTRACT_POOL_RATIO: f64 = 0.10;
const B5_0_DEFAULT_PRE_POP_ACCOUNTS: usize = 1_000_000;
const B5_0_DEFAULT_NUM_BLOCKS: usize = 10;
const B5_0_DEFAULT_TXS_PER_BLOCK: usize = 20_000;
const B5_0_DEFAULT_CONTRACT_RATIO: f64 = 0.30;
const B5_0_DEFAULT_CONTRACT_KV_PER_CONTRACT: usize = 64;
const B5_0_DEFAULT_ACTIVE_CONTRACT_POOL_RATIO: f64 = 0.10;
const PREPOP_CHUNK_SIZE: usize = 10_000;
const DEFAULT_SC_STORAGE_TRIE_CACHE_CAPACITY: usize = 200_000;
const DEFAULT_SC_PERSISTED_NODE_CACHE_CAPACITY: usize = 2_000_000;
const DEFAULT_SC_CROSS_BLOCK_SPARSE_MAX_LAG: i64 = 64;
const DEFAULT_ASYNC_PLAIN_QUEUE_CAPACITY: usize = 4;

fn pre_pop_accounts() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_PREPOP_ACCOUNTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PRE_POP_ACCOUNTS)
}

fn num_blocks() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_NUM_BLOCKS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_NUM_BLOCKS)
}

fn txs_per_block() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_TXS_PER_BLOCK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TXS_PER_BLOCK)
}

fn contract_ratio() -> f64 {
    std::env::var("MPTDB_PROVIDER_BENCH_CONTRACT_RATIO")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(DEFAULT_CONTRACT_RATIO)
}

fn contract_kv_per_contract() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_CONTRACT_KV_PER_CONTRACT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_CONTRACT_KV_PER_CONTRACT)
}

fn bench_parse_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok())
}

fn bench_parse_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok()).filter(|v| *v > 0)
}

fn bench_parse_i64(name: &str) -> Option<i64> {
    std::env::var(name).ok().and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0)
}

fn bench_sc_storage_trie_cache_capacity() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_SC_STORAGE_TRIE_CACHE_CAPACITY")
        .unwrap_or(DEFAULT_SC_STORAGE_TRIE_CACHE_CAPACITY)
}

fn bench_sc_persisted_node_cache_capacity() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_SC_PERSISTED_NODE_CACHE_CAPACITY")
        .unwrap_or(DEFAULT_SC_PERSISTED_NODE_CACHE_CAPACITY)
}

fn bench_sc_cross_block_sparse_max_lag() -> i64 {
    bench_parse_i64("MPTDB_PROVIDER_BENCH_SC_CROSS_BLOCK_SPARSE_MAX_LAG")
        .unwrap_or(DEFAULT_SC_CROSS_BLOCK_SPARSE_MAX_LAG)
}

fn b5_0_pre_pop_accounts() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_B5_0_PREPOP_ACCOUNTS")
        .unwrap_or(B5_0_DEFAULT_PRE_POP_ACCOUNTS)
}

fn b5_0_num_blocks() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_B5_0_NUM_BLOCKS").unwrap_or(B5_0_DEFAULT_NUM_BLOCKS)
}

fn b5_0_txs_per_block() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_B5_0_TXS_PER_BLOCK")
        .unwrap_or(B5_0_DEFAULT_TXS_PER_BLOCK)
}

fn b5_0_contract_ratio() -> f64 {
    bench_parse_f64("MPTDB_PROVIDER_BENCH_B5_0_CONTRACT_RATIO")
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(B5_0_DEFAULT_CONTRACT_RATIO)
}

fn b5_0_contract_kv_per_contract() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_B5_0_CONTRACT_KV_PER_CONTRACT")
        .unwrap_or(B5_0_DEFAULT_CONTRACT_KV_PER_CONTRACT)
}

fn b5_0_active_contract_pool_ratio() -> f64 {
    bench_parse_f64("MPTDB_PROVIDER_BENCH_B5_0_ACTIVE_CONTRACT_POOL_RATIO")
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(B5_0_DEFAULT_ACTIVE_CONTRACT_POOL_RATIO)
}

fn bench_sample_size() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_SAMPLE_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.max(10))
        .unwrap_or(DEFAULT_SAMPLE_SIZE)
}

fn bench_warmup_secs() -> u64 {
    std::env::var("MPTDB_PROVIDER_BENCH_WARMUP_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WARMUP_SECS)
}

fn bench_measurement_secs() -> u64 {
    std::env::var("MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MEASUREMENT_SECS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BenchMeasureMode {
    /// Per-block lifecycle only: EVM + DB write + root/commit (no setup/teardown).
    BlockLifecycle,
    /// End-to-end iteration including setup/teardown.
    EndToEnd,
}

fn bench_measure_mode() -> BenchMeasureMode {
    match std::env::var("MPTDB_PROVIDER_BENCH_MEASURE") {
        Ok(v) if matches!(v.as_str(), "end_to_end" | "wall") => BenchMeasureMode::EndToEnd,
        _ => BenchMeasureMode::BlockLifecycle,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MdbxWriteMode {
    /// `write_state` + `commit` (default integration path).
    Full,
    /// `write_state_changes` + `commit` (skip revert/history writes).
    PlainOnly,
    /// `commit` only (diagnostic no-op write path).
    Noop,
}

fn bench_mdbx_write_mode() -> MdbxWriteMode {
    match std::env::var("MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "plain" | "plain_only" => MdbxWriteMode::PlainOnly,
            "noop" => MdbxWriteMode::Noop,
            _ => MdbxWriteMode::Full,
        },
        Err(_) => MdbxWriteMode::Full,
    }
}

fn bench_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            // Backward-compatible fallback: any non-empty unknown value means enabled.
            _ => !raw.trim().is_empty(),
        },
        Err(_) => false,
    }
}

fn bench_trace_enabled() -> bool {
    bench_flag("MPTDB_PROVIDER_BENCH_TRACE")
}

fn bench_trace_iters() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_TRACE_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2)
}

fn bench_use_provider_reads() -> bool {
    bench_flag("MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS")
}

fn bench_enable_sc_prewarm() -> bool {
    bench_flag("MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM")
}

fn bench_sc_profile_enabled() -> bool {
    bench_flag("MPTDB_PROVIDER_BENCH_SC_PROFILE")
}

fn bench_sync_prewarm_after_block() -> bool {
    bench_flag("MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK")
}

fn bench_parallel_mdbx_write() -> bool {
    match std::env::var("MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE") {
        Ok(v) if matches!(v.trim(), "0" | "false" | "False" | "FALSE") => false,
        _ => true,
    }
}

fn bench_async_plain_materialization() -> bool {
    let parse = |v: &str| !matches!(v.trim(), "0" | "false" | "False" | "FALSE");
    if let Ok(v) = std::env::var("MPTDB_PROVIDER_BENCH_ASYNC_PLAIN_MATERIALIZATION") {
        return parse(&v);
    }
    if let Ok(v) = std::env::var("MPTDB_ASYNC_PLAIN_MATERIALIZATION") {
        return parse(&v);
    }
    true
}

fn bench_async_plain_queue_capacity() -> usize {
    bench_parse_usize("MPTDB_PROVIDER_BENCH_ASYNC_PLAIN_QUEUE_CAPACITY")
        .or_else(|| bench_parse_usize("MPTDB_ASYNC_PLAIN_QUEUE_CAPACITY"))
        .unwrap_or(DEFAULT_ASYNC_PLAIN_QUEUE_CAPACITY)
}

fn bench_flush_after_prepop() -> bool {
    match std::env::var("MPTDB_PROVIDER_BENCH_FLUSH_AFTER_PREPOP") {
        Ok(v) if matches!(v.trim(), "0" | "false" | "False" | "FALSE") => false,
        _ => true,
    }
}

// ── InMemoryCache — synthetic dataset builder (genesis + tx generation) ───────

#[derive(Debug, Clone)]
struct InMemoryCache {
    accounts: HashMap<Address, AccountInfo>,
    storage: HashMap<Address, HashMap<U256, U256>>,
    code_by_hash: HashMap<B256, revm::bytecode::Bytecode>,
}

impl InMemoryCache {
    fn new() -> Self {
        Self { accounts: HashMap::new(), storage: HashMap::new(), code_by_hash: HashMap::new() }
    }

    fn insert_account(&mut self, addr: Address, balance: U256, nonce: u64) {
        self.accounts.insert(
            addr,
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, code: None, account_id: None },
        );
    }

    fn insert_contract(&mut self, addr: Address, bytecode: Bytes) {
        let code_hash = keccak256(&bytecode);
        let bytecode = revm::bytecode::Bytecode::new_raw(bytecode);
        self.code_by_hash.insert(code_hash, bytecode.clone());
        self.accounts.insert(
            addr,
            AccountInfo {
                nonce: 1,
                balance: U256::ZERO,
                code_hash,
                code: Some(bytecode),
                account_id: None,
            },
        );
    }
}

#[derive(Clone)]
struct BenchStateProviderDb<P>(P);

impl<P> BenchStateProviderDb<P> {
    const fn new(provider: P) -> Self {
        Self(provider)
    }
}

impl<P: StateProvider> revm::Database for BenchStateProviderDb<P> {
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.0.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(self.0.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.0.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.0.block_hash(number)?.unwrap_or_default())
    }
}

impl<P: StateProvider> revm::DatabaseRef for BenchStateProviderDb<P> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.0.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(self.0.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.0.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.0.block_hash(number)?.unwrap_or_default())
    }
}

// ── InMemoryCache → BundleState ───────────────────────────────────────────────

fn cache_to_bundle(cache: &InMemoryCache) -> BundleState {
    use alloy_primitives::map::HashMap as PrimitivesHashMap;
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};

    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    for (&addr, info) in &cache.accounts {
        let mut storage = StorageWithOriginalValues::default();
        if let Some(slots) = cache.storage.get(&addr) {
            for (&slot_key, &value) in slots {
                let slot_b256 = B256::from(slot_key.to_be_bytes::<32>());
                storage.insert(slot_b256.into(), StorageSlot::new_changed(U256::ZERO, value));
            }
        }
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info.clone()),
                original_info: None,
                status: AccountStatus::Changed,
                storage,
            },
        );
    }
    // Include contract bytecodes so write_state populates the MDBX Bytecodes
    // table.  Without this, ERC20 calls land on accounts with no code and the
    // benchmark measures "call to empty account" rather than real storage updates.
    let contracts: PrimitivesHashMap<B256, revm::bytecode::Bytecode> =
        cache.code_by_hash.iter().map(|(&h, c)| (h, c.clone())).collect();
    BundleState { state, contracts, reverts: Default::default(), state_size: 0, reverts_size: 0 }
}

fn cache_to_bundle_chunk(cache: &InMemoryCache, addresses: &[Address]) -> BundleState {
    use alloy_primitives::map::HashMap as PrimitivesHashMap;
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};

    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();
    let mut contracts: PrimitivesHashMap<B256, revm::bytecode::Bytecode> =
        PrimitivesHashMap::default();

    for &addr in addresses {
        let Some(info) = cache.accounts.get(&addr) else { continue };
        let mut storage = StorageWithOriginalValues::default();
        if let Some(slots) = cache.storage.get(&addr) {
            for (&slot_key, &value) in slots {
                let slot_b256 = B256::from(slot_key.to_be_bytes::<32>());
                storage.insert(slot_b256.into(), StorageSlot::new_changed(U256::ZERO, value));
            }
        }
        if info.code_hash != KECCAK_EMPTY {
            if let Some(code) = cache.code_by_hash.get(&info.code_hash) {
                contracts.insert(info.code_hash, code.clone());
            }
        }
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: Some(info.clone()),
                original_info: None,
                status: AccountStatus::Changed,
                storage,
            },
        );
    }

    BundleState { state, contracts, reverts: Default::default(), state_size: 0, reverts_size: 0 }
}

fn sorted_prefill_addresses(cache: &InMemoryCache) -> Vec<Address> {
    let mut addrs: Vec<Address> = cache.accounts.keys().copied().collect();
    addrs.sort_unstable();
    addrs
}

// ── EVM execution ─────────────────────────────────────────────────────────────

fn execute_block_evm(
    state_provider: reth_storage_api::StateProviderBox,
    txs: impl Iterator<Item = TxEnv>,
    block_number: u64,
) -> BundleState {
    let state_db = State::builder()
        .with_database(BenchStateProviderDb::new(state_provider))
        .with_bundle_update()
        .build();
    let block_env = BlockEnv {
        number: U256::from(block_number),
        gas_limit: u64::MAX,
        basefee: 0,
        ..Default::default()
    };
    let mut ctx: MainnetContext<_> = Context::new(state_db, SpecId::CANCUN);
    ctx.cfg.disable_nonce_check = true;
    let ctx = ctx.with_block(block_env);
    let mut evm = ctx.build_mainnet();
    evm.transact_many_commit(txs).expect("EVM execution failed");
    let state_db = &mut evm.ctx.journaled_state.database;
    state_db.merge_transitions(BundleRetention::Reverts);
    state_db.take_bundle()
}

// ── Transaction generation ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PrefillDataset {
    all_addresses: Vec<Address>,
    eoa_addresses: Vec<Address>,
    contract_addresses: Vec<Address>,
}

fn setup_mixed_state(cache: &mut InMemoryCache, rng: &mut StdRng) -> PrefillDataset {
    setup_mixed_state_with_config(
        cache,
        rng,
        pre_pop_accounts(),
        txs_per_block(),
        contract_ratio(),
        contract_kv_per_contract(),
    )
}

fn setup_mixed_state_with_config(
    cache: &mut InMemoryCache,
    rng: &mut StdRng,
    total: usize,
    min_eoa_target: usize,
    contract_ratio: f64,
    kv_per_contract_target: usize,
) -> PrefillDataset {
    let min_eoa = min_eoa_target.max(1).min(total);
    let requested_contracts = ((total as f64) * contract_ratio).round() as usize;
    let max_contracts = total.saturating_sub(min_eoa);
    let contract_count = requested_contracts.min(max_contracts);
    let eoa_count = total.saturating_sub(contract_count);

    let mut all_addresses = Vec::with_capacity(total);
    let mut eoa_addresses = Vec::with_capacity(eoa_count);
    let mut contract_addresses = Vec::with_capacity(contract_count);
    let mut addr_buf = [0u8; 20];
    for idx in 0..total {
        rng.fill(&mut addr_buf);
        let addr = Address::from(addr_buf);
        all_addresses.push(addr);
        if idx < contract_count {
            contract_addresses.push(addr);
        } else {
            eoa_addresses.push(addr);
        }
    }

    for &addr in &eoa_addresses {
        cache.insert_account(addr, U256::from(INITIAL_BALANCE), 0);
    }

    for &addr in &contract_addresses {
        cache.insert_contract(addr, Bytes::from_static(&ERC20_RUNTIME_BYTECODE));
    }

    // Populate contract storage as ERC20-like balance mapping:
    // key = keccak256(abi.encode(holder, 0)).
    let kv_per_contract = kv_per_contract_target.min(eoa_addresses.len().max(1));
    for (i, &contract) in contract_addresses.iter().enumerate() {
        let slots = cache.storage.entry(contract).or_default();
        for j in 0..kv_per_contract {
            let holder = eoa_addresses[(i * kv_per_contract + j) % eoa_addresses.len()];
            slots.insert(erc20_balance_slot(holder), U256::from(1_000_000u64));
        }
    }

    PrefillDataset { all_addresses, eoa_addresses, contract_addresses }
}

fn generate_eth_block_txs(
    eoa_addresses: &[Address],
    receiver_addresses: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let blocks_n = num_blocks();
    // Cap to available senders: each block requires unique senders.
    // Without this cap the while-loop below would spin forever when
    // TXS_PER_BLOCK > eoa_addresses.len().
    let txs_n = txs_per_block().min(eoa_addresses.len());
    let mut nonces: HashMap<Address, u64> = eoa_addresses
        .iter()
        .map(|&addr| (addr, cache.accounts.get(&addr).map(|i| i.nonce).unwrap_or(0)))
        .collect();
    let mut blocks = Vec::with_capacity(blocks_n);
    for _ in 0..blocks_n {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(txs_n);
        while txs.len() < txs_n {
            let sender_idx = rng.random_range(0..eoa_addresses.len());
            if !used.insert(sender_idx) {
                continue;
            }
            let sender = eoa_addresses[sender_idx];
            let receiver = receiver_addresses[rng.random_range(0..receiver_addresses.len())];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            txs.push(TxEnv {
                caller: sender,
                kind: TxKind::Call(receiver),
                value: U256::from(1u64),
                gas_limit: 21_000,
                gas_price: 0,
                nonce,
                chain_id: Some(1),
                ..Default::default()
            });
            *nonces.entry(sender).or_insert(0) += 1;
        }
        blocks.push(txs);
    }
    blocks
}

// ── Helper: ExecutionOutcome wrapper ─────────────────────────────────────────

type Outcome = reth_execution_types::ExecutionOutcome<EthReceipt>;

fn make_outcome(bundle: BundleState, block_number: u64) -> Outcome {
    // One-block outcome with empty receipts; benchmark disables receipt writes.
    Outcome::new(bundle, vec![Vec::new()], block_number, vec![])
}

fn bench_state_write_config() -> StateWriteConfig {
    // Bench focuses on state DB cost. Receipts are not part of this benchmark.
    StateWriteConfig { write_receipts: false, write_account_changesets: false }
}

// ── mptdb backend ─────────────────────────────────────────────────────────────

fn run_mptdb_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    label: &str,
) {
    use reth_storage_api::{StateWriter, TrieWriter};
    use reth_trie_db::DatabaseStateRoot as _;

    b.iter_custom(|iters| {
        let measure_mode = bench_measure_mode();
        let trace = bench_trace_enabled();
        let trace_iters = bench_trace_iters();
        let use_provider_reads = bench_use_provider_reads();
        let enable_sc_prewarm = bench_enable_sc_prewarm();
        let enable_sc_profile = bench_sc_profile_enabled();
        let sync_prewarm_after_block = bench_sync_prewarm_after_block();
        let parallel_mdbx_write = bench_parallel_mdbx_write();
        let async_plain_materialization = bench_async_plain_materialization();
        let async_plain_queue_capacity =
            if async_plain_materialization { bench_async_plain_queue_capacity() } else { 1 };
        let mdbx_write_mode = bench_mdbx_write_mode();
        let sc_storage_trie_cache_capacity = bench_sc_storage_trie_cache_capacity();
        let sc_persisted_node_cache_capacity = bench_sc_persisted_node_cache_capacity();
        let sc_cross_block_sparse_max_lag = bench_sc_cross_block_sparse_max_lag();
        let mut exec_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        let mut setup_total = Duration::ZERO;
        let mut teardown_total = Duration::ZERO;
        let mut evm_total = Duration::ZERO;
        let mut sc_write_total = Duration::ZERO;
        let mut write_total = Duration::ZERO; // wall of SC+MDBX phase
        let mut prepop_total = Duration::ZERO;
        let mut mdbx_send_wait_total = Duration::ZERO;
        let mut mdbx_send_wait_max_total = Duration::ZERO;
        let mut mdbx_pending_max_total: usize = 0;
        let mut sc_apply_total = Duration::ZERO;
        let mut sc_collect_dirty_total = Duration::ZERO;
        let mut sc_account_checkout_total = Duration::ZERO;
        let mut sc_sparse_factory_build_total = Duration::ZERO;
        let mut sc_sparse_account_proof_total = Duration::ZERO;
        let mut sc_sparse_apply_changes_total = Duration::ZERO;
        let mut sc_sparse_factory_dirty_accounts_total: u64 = 0;
        let mut sc_sparse_factory_storage_accounts_total: u64 = 0;
        let mut sc_sparse_factory_segment_lookups_total: u64 = 0;
        let mut sc_sparse_factory_segment_hits_total: u64 = 0;
        let mut sc_sparse_factory_segment_miss_no_store_total: u64 = 0;
        let mut sc_sparse_factory_segment_miss_total: u64 = 0;
        let mut sc_sparse_factory_segment_root_mismatch_total: u64 = 0;
        let mut sc_sparse_factory_tier3_attempts_total: u64 = 0;
        let mut sc_sparse_factory_tier3_hits_total: u64 = 0;
        let mut sc_sparse_factory_tier12_attempts_total: u64 = 0;
        let mut sc_sparse_factory_cross_reuse_accounts_total: u64 = 0;
        let mut sc_sparse_factory_cross_missing_slots_total: u64 = 0;
        let mut sc_sparse_factory_cross_missing_proof_slots_total: u64 = 0;
        let mut sc_trie_load_total = Duration::ZERO;
        let mut sc_slot_updates_total = Duration::ZERO;
        let mut sc_storage_roots_total = Duration::ZERO;
        let mut sc_account_updates_total = Duration::ZERO;
        let mut sc_account_root_total = Duration::ZERO;
        let mut sc_wal_total = Duration::ZERO;
        let mut sc_commit_total = Duration::ZERO;
        let mut outcome_accounts_total: u64 = 0;
        let mut outcome_storage_accounts_total: u64 = 0;
        let mut outcome_storage_slots_total: u64 = 0;

        for iter_idx in 0..iters {
            let use_provider_reads_effective = if async_plain_materialization {
                true
            } else {
                use_provider_reads
            };
            let iter_start = Instant::now();
            let mut open_sc = Duration::ZERO;
            let mut pre_pop = Duration::ZERO;
            let mut evm_phase = Duration::ZERO;
            let mut sc_write_phase = Duration::ZERO;
            let mut write_phase = Duration::ZERO; // wall of SC+MDBX phase
            let mut mdbx_send_wait_phase = Duration::ZERO;
            let mut mdbx_send_wait_max_phase = Duration::ZERO;
            let mut pending_mdbx_jobs_max: usize = 0;
            let mut drop_phase = Duration::ZERO;
            let mut tmp_drop = Duration::ZERO;
            let mut sc_apply_phase = Duration::ZERO;
            let mut sc_collect_dirty_phase = Duration::ZERO;
            let mut sc_account_checkout_phase = Duration::ZERO;
            let mut sc_sparse_factory_build_phase = Duration::ZERO;
            let mut sc_sparse_account_proof_phase = Duration::ZERO;
            let mut sc_sparse_apply_changes_phase = Duration::ZERO;
            let mut sc_sparse_factory_dirty_accounts_phase: u64 = 0;
            let mut sc_sparse_factory_storage_accounts_phase: u64 = 0;
            let mut sc_sparse_factory_segment_lookups_phase: u64 = 0;
            let mut sc_sparse_factory_segment_hits_phase: u64 = 0;
            let mut sc_sparse_factory_segment_miss_no_store_phase: u64 = 0;
            let mut sc_sparse_factory_segment_miss_phase: u64 = 0;
            let mut sc_sparse_factory_segment_root_mismatch_phase: u64 = 0;
            let mut sc_sparse_factory_tier3_attempts_phase: u64 = 0;
            let mut sc_sparse_factory_tier3_hits_phase: u64 = 0;
            let mut sc_sparse_factory_tier12_attempts_phase: u64 = 0;
            let mut sc_sparse_factory_cross_reuse_accounts_phase: u64 = 0;
            let mut sc_sparse_factory_cross_missing_slots_phase: u64 = 0;
            let mut sc_sparse_factory_cross_missing_proof_slots_phase: u64 = 0;
            let mut sc_trie_load_phase = Duration::ZERO;
            let mut sc_slot_updates_phase = Duration::ZERO;
            let mut sc_storage_roots_phase = Duration::ZERO;
            let mut sc_account_updates_phase = Duration::ZERO;
            let mut sc_account_root_phase = Duration::ZERO;
            let mut sc_wal_phase = Duration::ZERO;
            let mut sc_commit_phase = Duration::ZERO;
            let mut outcome_accounts_phase: u64 = 0;
            let mut outcome_storage_accounts_phase: u64 = 0;
            let mut outcome_storage_slots_phase: u64 = 0;

            let iter_dir = TempDir::new().unwrap();

            // Open mptdb
            let mut sc_config = MptConfig::default();
            sc_config.storage_trie_cache_capacity = sc_storage_trie_cache_capacity;
            sc_config.persisted_node_cache_capacity = sc_persisted_node_cache_capacity;
            sc_config.cross_block_sparse_max_lag = sc_cross_block_sparse_max_lag;
            let t = Instant::now();
            let sc = Arc::new(Mutex::new(
                MptCommitStore::open_with_config(iter_dir.path(), false, sc_config).expect("open SC"),
            ));
            open_sc += t.elapsed();

            let sc_writer = MptDbStateWriter::<EthReceipt>::new(Arc::clone(&sc));
            let noop_block_id: Arc<dyn BlockIdReader + Send + Sync> =
                Arc::new(reth_storage_api::noop::NoopProvider::default());
            // SC prewarm dispatcher: triggered AFTER each SC commit (block boundary),
            // not from the EVM read hot path.  Prewarms storage tries for accounts
            // touched in the committed block, so the NEXT block's SC operations find
            // warm L2 cache entries.
            // SC prewarm is independent of use_provider_reads: it warms SC's
            // L2 trie cache after each commit, which benefits SC writes regardless
            // of whether EVM reads go through MptDbStateProvider or raw MDBX.
            let sc_prewarm = if enable_sc_prewarm {
                Some(
                    ScPrewarmDispatcher::spawn(Arc::clone(&sc), 16_384, 256)
                        .expect("spawn sc prewarm worker"),
                )
            } else {
                None
            };

            // Create MDBX ProviderFactory — same as the reth_mdbx path.
            // EVM reads go directly from MDBX (no Mutex wrapper) to match the
            // production path where reth writes PlainState and SC reads MDBX
            // via the StateProviderOverride default_provider.
            use reth_provider::test_utils::create_test_provider_factory;
            let mdbx_factory = create_test_provider_factory();

            let genesis = cache_to_bundle(cache);
            let prefill_addresses = sorted_prefill_addresses(cache);
            let t = Instant::now();
            // Genesis → MDBX (chunked prefill, aligned with B4.x-style prefill semantics)
            for chunk in prefill_addresses.chunks(PREPOP_CHUNK_SIZE) {
                let provider = mdbx_factory.provider_rw().expect("genesis rw");
                let chunk_bundle = cache_to_bundle_chunk(cache, chunk);
                let chunk_outcome = make_outcome(chunk_bundle, 0);
                provider
                    .write_state(
                        &chunk_outcome,
                        OriginalValuesKnown::Yes,
                        bench_state_write_config(),
                    )
                    .expect("genesis mdbx write_state");
                let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                    chunk_outcome.state().state(),
                );
                let (_, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                    provider.tx_ref(),
                    &hashed.into_sorted(),
                )
                .expect("genesis trie root");
                provider.write_trie_updates(trie_updates).expect("genesis trie updates");
                provider.commit().expect("genesis mdbx commit");
            }
            // Genesis → SC
            sc_writer.pre_populate(&genesis, 0).expect("pre_populate SC");
            if bench_flush_after_prepop() {
                sc.lock().flush_persist().expect("flush persist after pre_populate");
            }
            pre_pop += t.elapsed();

            // Default mode: SC and MDBX writes run in parallel.
            // A/B mode (MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=0):
            // run MDBX write serially after SC commit to test contention impact.
            type JobOutcome = Arc<Outcome>;
            if async_plain_materialization && !parallel_mdbx_write {
                panic!(
                    "MPTDB_PROVIDER_BENCH_ASYNC_PLAIN_MATERIALIZATION=1 requires \
MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=1"
                );
            }
            let mut job_tx: Option<std::sync::mpsc::SyncSender<JobOutcome>> = None;
            let mut done_rx: Option<std::sync::mpsc::Receiver<()>> = None;
            let mut mdbx_worker: Option<std::thread::JoinHandle<()>> = None;
            let mut pending_mdbx_jobs: usize = 0;
            if parallel_mdbx_write {
                let (tx, job_rx) =
                    std::sync::mpsc::sync_channel::<JobOutcome>(async_plain_queue_capacity);
                let (done_tx, rx) = std::sync::mpsc::channel::<()>();
                let factory = mdbx_factory.clone();
                let write_mode = mdbx_write_mode;
                let worker = std::thread::spawn(move || {
                    while let Ok(outcome) = job_rx.recv() {
                        let rw = factory.provider_rw().expect("mdbx rw");
                        match write_mode {
                            MdbxWriteMode::Full => {
                                // write_state writes PlainAccountState, PlainStorageState,
                                // HashedAccountState, AccountChangeSets, StorageChangeSets, etc.
                                // (see DatabaseProvider::write_state for the full set).
                                // Intentionally omit overlay_root_with_updates + write_trie_updates:
                                // SC owns state root; MDBX AccountsTrie/StoragesTrie are not needed
                                // since EVM reads use PlainState, not the trie tables.
                                rw.write_state(
                                    outcome.as_ref(),
                                    OriginalValuesKnown::Yes,
                                    bench_state_write_config(),
                                )
                                .expect("mdbx write_state");
                            }
                            MdbxWriteMode::PlainOnly => {
                                let plain = outcome.state().to_plain_state(OriginalValuesKnown::Yes);
                                rw.write_state_changes(plain).expect("mdbx write_state_changes");
                            }
                            MdbxWriteMode::Noop => {}
                        }
                        rw.commit().expect("mdbx commit");
                        done_tx.send(()).expect("done signal");
                    }
                });
                job_tx = Some(tx);
                done_rx = Some(rx);
                mdbx_worker = Some(worker);
            }

            let exec_start = Instant::now();
            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let t_evm = Instant::now();
                let bundle = if use_provider_reads_effective {
                    let version = sc.lock().version().max(0);
                    let fallback: Arc<dyn StateProvider + Send + Sync> =
                        SyncProvider::new(mdbx_factory.latest().expect("mdbx latest"));
                    let provider = MptDbStateProvider::new(
                        Arc::clone(&sc),
                        version,
                        fallback,
                        Arc::clone(&noop_block_id),
                    );
                    // No prewarm in the EVM read hot path — prewarm is triggered
                    // after SC commit (see below) so it operates at block granularity.
                    execute_block_evm(Box::new(provider), txs.clone().into_iter(), blk_idx as u64 + 1)
                } else {
                    // Default benchmark mode: direct MDBX reads (no provider wrapper).
                    let db_provider = mdbx_factory.latest().expect("mdbx latest");
                    execute_block_evm(db_provider, txs.clone().into_iter(), blk_idx as u64 + 1)
                };
                evm_phase += t_evm.elapsed();

                // Wrap outcome in Arc — MDBX worker and SC commit share it
                // without any large-object clone.
                let t_wr = Instant::now();
                let outcome = Arc::new(make_outcome(bundle, blk_idx as u64 + 1));
                let (changed_accounts, storage_accounts, storage_slots) = {
                    let state = outcome.state().state();
                    let changed = state.len() as u64;
                    let storage = state.values().filter(|acc| !acc.storage.is_empty()).count() as u64;
                    let slots = state.values().map(|acc| acc.storage.len() as u64).sum::<u64>();
                    (changed, storage, slots)
                };
                outcome_accounts_phase += changed_accounts;
                outcome_storage_accounts_phase += storage_accounts;
                outcome_storage_slots_phase += storage_slots;
                if let Some(tx) = job_tx.as_ref() {
                    let send_start = Instant::now();
                    tx.send(Arc::clone(&outcome)).expect("send to mdbx worker");
                    let send_wait = send_start.elapsed();
                    mdbx_send_wait_phase += send_wait;
                    mdbx_send_wait_max_phase = mdbx_send_wait_max_phase.max(send_wait);
                    pending_mdbx_jobs = pending_mdbx_jobs.saturating_add(1);
                    pending_mdbx_jobs_max = pending_mdbx_jobs_max.max(pending_mdbx_jobs);
                }

                // SC commit on main thread (uses rayon internally).
                let t_sc = Instant::now();
                if enable_sc_profile {
                    sc_writer
                        .apply_execution_outcome(outcome.as_ref())
                        .expect("sc apply_execution_outcome");
                    let ((_version, _root), profile) =
                        sc.lock().commit_with_profile().expect("sc commit_with_profile");
                    sc_apply_phase += profile.apply_bundle_state;
                    sc_collect_dirty_phase += profile.apply_collect_dirty_accounts;
                    sc_account_checkout_phase += profile.apply_account_trie_checkout;
                    sc_sparse_factory_build_phase += profile.sparse_apply_factory_build;
                    sc_sparse_account_proof_phase += profile.sparse_apply_account_proof;
                    sc_sparse_apply_changes_phase += profile.sparse_apply_apply_changes;
                    sc_sparse_factory_dirty_accounts_phase += profile.sparse_factory_dirty_accounts;
                    sc_sparse_factory_storage_accounts_phase += profile.sparse_factory_storage_accounts;
                    sc_sparse_factory_segment_lookups_phase += profile.sparse_factory_segment_lookups;
                    sc_sparse_factory_segment_hits_phase += profile.sparse_factory_segment_hits;
                    sc_sparse_factory_segment_miss_no_store_phase +=
                        profile.sparse_factory_segment_miss_no_store;
                    sc_sparse_factory_segment_miss_phase += profile.sparse_factory_segment_miss;
                    sc_sparse_factory_segment_root_mismatch_phase +=
                        profile.sparse_factory_segment_root_mismatch;
                    sc_sparse_factory_tier3_attempts_phase += profile.sparse_factory_tier3_attempts;
                    sc_sparse_factory_tier3_hits_phase += profile.sparse_factory_tier3_hits;
                    sc_sparse_factory_tier12_attempts_phase += profile.sparse_factory_tier12_attempts;
                    sc_sparse_factory_cross_reuse_accounts_phase +=
                        profile.sparse_factory_cross_reuse_accounts;
                    sc_sparse_factory_cross_missing_slots_phase +=
                        profile.sparse_factory_cross_missing_slots;
                    sc_sparse_factory_cross_missing_proof_slots_phase +=
                        profile.sparse_factory_cross_missing_proof_slots;
                    sc_trie_load_phase += profile.apply_get_or_load_storage_tries;
                    sc_slot_updates_phase += profile.apply_storage_slot_updates;
                    sc_storage_roots_phase += profile.storage_roots;
                    sc_account_updates_phase += profile.account_updates;
                    sc_account_root_phase += profile.account_root_and_blobs;
                    sc_wal_phase += profile.wal_append;
                    sc_commit_phase += profile.total_commit;
                } else {
                    sc_writer
                        .write_state(
                            outcome.as_ref(),
                            OriginalValuesKnown::Yes,
                            bench_state_write_config(),
                        )
                        .expect("sc write_state");
                }
                sc_write_phase += t_sc.elapsed();

                // In parallel mode, sync mode waits MDBX per block.
                // Async mode defers waiting to iteration end to model WAL-first
                // plain-state materialization outside block hot path.
                // In serial mode, write MDBX after SC commit.
                if let Some(rx) = done_rx.as_ref() {
                    if !async_plain_materialization {
                        rx.recv().expect("mdbx done");
                        pending_mdbx_jobs = pending_mdbx_jobs.saturating_sub(1);
                    }
                } else {
                    let rw = mdbx_factory.provider_rw().expect("mdbx rw");
                    match mdbx_write_mode {
                        MdbxWriteMode::Full => {
                            rw.write_state(
                                outcome.as_ref(),
                                OriginalValuesKnown::Yes,
                                bench_state_write_config(),
                            )
                            .expect("mdbx write_state");
                        }
                        MdbxWriteMode::PlainOnly => {
                            let plain = outcome.state().to_plain_state(OriginalValuesKnown::Yes);
                            rw.write_state_changes(plain).expect("mdbx write_state_changes");
                        }
                        MdbxWriteMode::Noop => {}
                    }
                    rw.commit().expect("mdbx commit");
                }
                write_phase += t_wr.elapsed();

                // Enqueue storage-touching accounts for background SC prewarm AFTER
                // write_phase has been measured.  This avoids polluting the block
                // lifecycle timing with enqueue overhead.
                // Filter to accounts with storage changes only: EOA senders have
                // empty storage and would trigger expensive account-MPT traversals
                // in prewarm_storage_trie_by_hashed_address.
                if let Some(ref prewarm) = sc_prewarm {
                    for (addr, account) in outcome.state().state.iter() {
                        if !account.storage.is_empty() {
                            prewarm.enqueue_address(*addr);
                        }
                    }
                }
                // Diagnostic mode: run provider-equivalent prewarm synchronously and
                // force publish before next block. This mirrors B4.8's per-block
                // sync prewarm + flush behavior.
                if sync_prewarm_after_block {
                    let mut store = sc.lock();
                    store.maybe_refresh_published_view_for_prewarm();
                    for (addr, account) in outcome.state().state.iter() {
                        if !account.storage.is_empty() {
                            let _ = store.prewarm_storage_trie_by_hashed_address(keccak256(addr.as_slice()));
                        }
                    }
                    store.flush_persist().expect("flush persist after block sync prewarm");
                }
            }
            exec_total += exec_start.elapsed();

            if async_plain_materialization {
                if let Some(rx) = done_rx.as_ref() {
                    for _ in 0..pending_mdbx_jobs {
                        rx.recv().expect("mdbx done (async drain)");
                        pending_mdbx_jobs = pending_mdbx_jobs.saturating_sub(1);
                    }
                }
            }
            drop(job_tx); // signal MDBX worker to exit
            if let Some(worker) = mdbx_worker.take() {
                worker.join().expect("mdbx worker");
            }
            // Drop prewarm dispatcher BEFORE dropping SC.  ScPrewarmDispatcher::Drop
            // closes tx (signals worker exit) then joins the thread, guaranteeing
            // the worker has stopped and there is no cross-iteration backlog.
            drop(sc_prewarm);

            let t = Instant::now();
            drop(sc_writer);
            drop(sc);
            drop(mdbx_factory);
            drop_phase += t.elapsed();
            let t = Instant::now();
            drop(iter_dir);
            tmp_drop += t.elapsed();

            setup_total += open_sc + pre_pop;
            prepop_total += pre_pop;
            mdbx_send_wait_total += mdbx_send_wait_phase;
            mdbx_send_wait_max_total = mdbx_send_wait_max_total.max(mdbx_send_wait_max_phase);
            mdbx_pending_max_total += pending_mdbx_jobs_max;
            evm_total += evm_phase;
            sc_write_total += sc_write_phase;
            write_total += write_phase;
            teardown_total += drop_phase + tmp_drop;
            wall_total += iter_start.elapsed();
            sc_apply_total += sc_apply_phase;
            sc_collect_dirty_total += sc_collect_dirty_phase;
            sc_account_checkout_total += sc_account_checkout_phase;
            sc_sparse_factory_build_total += sc_sparse_factory_build_phase;
            sc_sparse_account_proof_total += sc_sparse_account_proof_phase;
            sc_sparse_apply_changes_total += sc_sparse_apply_changes_phase;
            sc_sparse_factory_dirty_accounts_total += sc_sparse_factory_dirty_accounts_phase;
            sc_sparse_factory_storage_accounts_total += sc_sparse_factory_storage_accounts_phase;
            sc_sparse_factory_segment_lookups_total += sc_sparse_factory_segment_lookups_phase;
            sc_sparse_factory_segment_hits_total += sc_sparse_factory_segment_hits_phase;
            sc_sparse_factory_segment_miss_no_store_total +=
                sc_sparse_factory_segment_miss_no_store_phase;
            sc_sparse_factory_segment_miss_total += sc_sparse_factory_segment_miss_phase;
            sc_sparse_factory_segment_root_mismatch_total +=
                sc_sparse_factory_segment_root_mismatch_phase;
            sc_sparse_factory_tier3_attempts_total += sc_sparse_factory_tier3_attempts_phase;
            sc_sparse_factory_tier3_hits_total += sc_sparse_factory_tier3_hits_phase;
            sc_sparse_factory_tier12_attempts_total += sc_sparse_factory_tier12_attempts_phase;
            sc_sparse_factory_cross_reuse_accounts_total +=
                sc_sparse_factory_cross_reuse_accounts_phase;
            sc_sparse_factory_cross_missing_slots_total +=
                sc_sparse_factory_cross_missing_slots_phase;
            sc_sparse_factory_cross_missing_proof_slots_total +=
                sc_sparse_factory_cross_missing_proof_slots_phase;
            sc_trie_load_total += sc_trie_load_phase;
            sc_slot_updates_total += sc_slot_updates_phase;
            sc_storage_roots_total += sc_storage_roots_phase;
            sc_account_updates_total += sc_account_updates_phase;
            sc_account_root_total += sc_account_root_phase;
            sc_wal_total += sc_wal_phase;
            sc_commit_total += sc_commit_phase;
            outcome_accounts_total += outcome_accounts_phase;
            outcome_storage_accounts_total += outcome_storage_accounts_phase;
            outcome_storage_slots_total += outcome_storage_slots_phase;

            if trace && (iter_idx as usize) < trace_iters {
                eprintln!(
                    "[trace][{}][iter {}] setup(open_sc={:.2?}, pre_pop={:.2?}) exec(evm={:.2?}, sc_write={:.2?}, write_wall={:.2?}, total={:.2?}) teardown(drop={:.2?}, tmp_drop={:.2?}) wall={:.2?}",
                    label,
                    iter_idx + 1,
                    open_sc,
                    pre_pop,
                    evm_phase,
                    sc_write_phase,
                    write_phase,
                    evm_phase + write_phase,
                    drop_phase,
                    tmp_drop,
                    iter_start.elapsed()
                );
                if parallel_mdbx_write {
                    eprintln!(
                        "[trace][{}][iter {}] mdbx_queue(capacity={}, pending_max={}, send_wait_total={:.2?}, send_wait_max={:.2?})",
                        label,
                        iter_idx + 1,
                        async_plain_queue_capacity,
                        pending_mdbx_jobs_max,
                        mdbx_send_wait_phase,
                        mdbx_send_wait_max_phase,
                    );
                }
                if enable_sc_profile {
                    eprintln!(
                        "[trace][{}][iter {}] sc_profile(apply={:.2?}, collect_dirty={:.2?}, account_checkout={:.2?}, sparse_factory_build={:.2?}, sparse_account_proof={:.2?}, sparse_apply_changes={:.2?}, trie_load={:.2?}, slot_updates={:.2?}, storage_roots={:.2?}, account_updates={:.2?}, account_root={:.2?}, wal={:.2?}, total_commit={:.2?}, changed_accounts={}, storage_accounts={}, storage_slots={})",
                        label,
                        iter_idx + 1,
                        sc_apply_phase,
                        sc_collect_dirty_phase,
                        sc_account_checkout_phase,
                        sc_sparse_factory_build_phase,
                        sc_sparse_account_proof_phase,
                        sc_sparse_apply_changes_phase,
                        sc_trie_load_phase,
                        sc_slot_updates_phase,
                        sc_storage_roots_phase,
                        sc_account_updates_phase,
                        sc_account_root_phase,
                        sc_wal_phase,
                        sc_commit_phase,
                        outcome_accounts_phase,
                        outcome_storage_accounts_phase,
                        outcome_storage_slots_phase,
                    );
                    eprintln!(
                        "[trace][{}][iter {}] sparse_factory_stats(dirty={} storage={} seg={}/{} miss_no_store={} miss={} root_mismatch={} t3={}/{} t12={} cross_reuse={} cross_missing_slots={} cross_missing_proof_slots={})",
                        label,
                        iter_idx + 1,
                        sc_sparse_factory_dirty_accounts_phase,
                        sc_sparse_factory_storage_accounts_phase,
                        sc_sparse_factory_segment_hits_phase,
                        sc_sparse_factory_segment_lookups_phase,
                        sc_sparse_factory_segment_miss_no_store_phase,
                        sc_sparse_factory_segment_miss_phase,
                        sc_sparse_factory_segment_root_mismatch_phase,
                        sc_sparse_factory_tier3_hits_phase,
                        sc_sparse_factory_tier3_attempts_phase,
                        sc_sparse_factory_tier12_attempts_phase,
                        sc_sparse_factory_cross_reuse_accounts_phase,
                        sc_sparse_factory_cross_missing_slots_phase,
                        sc_sparse_factory_cross_missing_proof_slots_phase,
                    );
                }
            }
        }
        eprintln!(
            "[{}] avg/blk(block-lifecycle): {:.2?}",
            label,
            exec_total / (iters * block_txs.len() as u64) as u32
        );
        eprintln!(
            "[{}] avg/iter breakdown: setup={:.2?} (pre_pop={:.2?}) evm={:.2?} sc_write={:.2?} write_wall={:.2?} teardown={:.2?}",
            label,
            setup_total / iters as u32,
            prepop_total / iters as u32,
            evm_total / iters as u32,
            sc_write_total / iters as u32,
            write_total / iters as u32,
            teardown_total / iters as u32,
        );
        if parallel_mdbx_write {
            eprintln!(
                "[{}] avg/mdbx_queue per-iter: capacity={} pending_max={} send_wait_total={:.2?} send_wait_max={:.2?}",
                label,
                async_plain_queue_capacity,
                mdbx_pending_max_total / iters as usize,
                mdbx_send_wait_total / iters as u32,
                mdbx_send_wait_max_total,
            );
        }
        eprintln!("[{}] avg/iter(end-to-end): {:.2?}", label, wall_total / iters as u32);
        eprintln!(
            "[{}] read_mode: provider_reads(req={},effective={}) async_plain_materialization={} async_plain_queue_capacity={} sc_prewarm={} sync_prewarm_after_block={} parallel_mdbx_write={} mdbx_write_mode={:?} sc_storage_cache={} sc_persisted_cache={} sc_cross_lag={}",
            label,
            use_provider_reads,
            if async_plain_materialization { true } else { use_provider_reads },
            async_plain_materialization,
            async_plain_queue_capacity,
            enable_sc_prewarm,
            sync_prewarm_after_block,
            parallel_mdbx_write,
            mdbx_write_mode,
            sc_storage_trie_cache_capacity,
            sc_persisted_node_cache_capacity,
            sc_cross_block_sparse_max_lag
        );
        if enable_sc_profile {
            let total_blocks = (iters * block_txs.len() as u64) as u32;
            eprintln!(
                "[{}] avg/sc_profile per-block: apply={:.2?} collect_dirty={:.2?} account_checkout={:.2?} sparse_factory_build={:.2?} sparse_account_proof={:.2?} sparse_apply_changes={:.2?} trie_load={:.2?} slot_updates={:.2?} storage_roots={:.2?} account_updates={:.2?} account_root={:.2?} wal={:.2?} total_commit={:.2?} changed_accounts={} storage_accounts={} storage_slots={}",
                label,
                sc_apply_total / total_blocks,
                sc_collect_dirty_total / total_blocks,
                sc_account_checkout_total / total_blocks,
                sc_sparse_factory_build_total / total_blocks,
                sc_sparse_account_proof_total / total_blocks,
                sc_sparse_apply_changes_total / total_blocks,
                sc_trie_load_total / total_blocks,
                sc_slot_updates_total / total_blocks,
                sc_storage_roots_total / total_blocks,
                sc_account_updates_total / total_blocks,
                sc_account_root_total / total_blocks,
                sc_wal_total / total_blocks,
                sc_commit_total / total_blocks,
                outcome_accounts_total / total_blocks as u64,
                outcome_storage_accounts_total / total_blocks as u64,
                outcome_storage_slots_total / total_blocks as u64,
            );
            eprintln!(
                "[{}] avg/sparse_factory per-block: dirty={} storage={} seg={}/{} miss_no_store={} miss={} root_mismatch={} t3={}/{} t12={} cross_reuse={} cross_missing_slots={} cross_missing_proof_slots={}",
                label,
                sc_sparse_factory_dirty_accounts_total / total_blocks as u64,
                sc_sparse_factory_storage_accounts_total / total_blocks as u64,
                sc_sparse_factory_segment_hits_total / total_blocks as u64,
                sc_sparse_factory_segment_lookups_total / total_blocks as u64,
                sc_sparse_factory_segment_miss_no_store_total / total_blocks as u64,
                sc_sparse_factory_segment_miss_total / total_blocks as u64,
                sc_sparse_factory_segment_root_mismatch_total / total_blocks as u64,
                sc_sparse_factory_tier3_hits_total / total_blocks as u64,
                sc_sparse_factory_tier3_attempts_total / total_blocks as u64,
                sc_sparse_factory_tier12_attempts_total / total_blocks as u64,
                sc_sparse_factory_cross_reuse_accounts_total / total_blocks as u64,
                sc_sparse_factory_cross_missing_slots_total / total_blocks as u64,
                sc_sparse_factory_cross_missing_proof_slots_total / total_blocks as u64,
            );
        }
        eprintln!("[{}] criterion measure mode: {:?}", label, measure_mode);
        if matches!(measure_mode, BenchMeasureMode::EndToEnd) {
            wall_total
        } else {
            exec_total
        }
    });
}

// ── reth MDBX backend ─────────────────────────────────────────────────────────

fn run_reth_mdbx_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    label: &str,
) {
    use reth_provider::{test_utils::create_test_provider_factory, StateWriter};
    use reth_storage_api::TrieWriter;
    use reth_trie::HashedPostState;
    use reth_trie_db::DatabaseStateRoot;

    b.iter_custom(|iters| {
        let measure_mode = bench_measure_mode();
        let trace = bench_trace_enabled();
        let trace_iters = bench_trace_iters();
        let mut exec_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        let mut setup_total = Duration::ZERO;
        let mut teardown_total = Duration::ZERO;
        let mut evm_total = Duration::ZERO;
        let mut write_total = Duration::ZERO;
        let mut root_total = Duration::ZERO;
        let mut genesis_total = Duration::ZERO;

        for iter_idx in 0..iters {
            let iter_start = Instant::now();
            let mut setup = Duration::ZERO;
            let mut genesis = Duration::ZERO;
            let mut evm_phase = Duration::ZERO;
            let mut write_phase = Duration::ZERO;
            let mut root_phase = Duration::ZERO;
            let mut teardown = Duration::ZERO;

            let t = Instant::now();
            let factory = create_test_provider_factory();
            setup += t.elapsed();

            // Pre-populate genesis state + compute genesis trie root
            let prefill_addresses = sorted_prefill_addresses(cache);
            {
                let t = Instant::now();
                for chunk in prefill_addresses.chunks(PREPOP_CHUNK_SIZE) {
                    let provider = factory.provider_rw().expect("provider_rw");
                    let chunk_bundle = cache_to_bundle_chunk(cache, chunk);
                    let chunk_outcome = make_outcome(chunk_bundle, 0);
                    provider
                        .write_state(
                            &chunk_outcome,
                            OriginalValuesKnown::Yes,
                            bench_state_write_config(),
                        )
                        .expect("genesis write_state");
                    // Engine mode: compute state root using DatabaseStateRoot on MDBX tx
                    let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                        chunk_outcome.state().state.par_iter(),
                    );
                    let (_root, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                        provider.tx_ref(),
                        &hashed.into_sorted(),
                    )
                    .expect("genesis state_root");
                    provider.write_trie_updates(trie_updates).expect("trie_updates");
                    provider.commit().expect("genesis commit");
                }
                genesis += t.elapsed();
            }

            let exec_start = Instant::now();
            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let t_evm = Instant::now();
                let state_provider = factory.latest().expect("latest state provider");
                let bundle =
                    execute_block_evm(state_provider, txs.clone().into_iter(), blk_idx as u64 + 1);
                evm_phase += t_evm.elapsed();
                let provider = factory.provider_rw().expect("provider_rw");

                // Wrap in Arc so write_state and hashing share the same allocation.
                let outcome = Arc::new(make_outcome(bundle, blk_idx as u64 + 1));

                // 1. Persist execution output (PlainState + HashedState)
                let t_wr = Instant::now();
                provider
                    .write_state(
                        outcome.as_ref(),
                        OriginalValuesKnown::Yes,
                        bench_state_write_config(),
                    )
                    .expect("write_state");
                write_phase += t_wr.elapsed();

                // 2. Engine mode: compute state root per block
                let t_root = Instant::now();
                let hashed =
                    HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                        outcome.state().state.par_iter(),
                    );
                let (_root, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                    provider.tx_ref(),
                    &hashed.into_sorted(),
                )
                .expect("state_root_with_updates");
                provider.write_trie_updates(trie_updates).expect("write_trie_updates");
                provider.commit().expect("commit");
                root_phase += t_root.elapsed();
            }
            exec_total += exec_start.elapsed();
            let t = Instant::now();
            drop(factory);
            teardown += t.elapsed();

            setup_total += setup + genesis;
            genesis_total += genesis;
            evm_total += evm_phase;
            write_total += write_phase;
            root_total += root_phase;
            teardown_total += teardown;
            wall_total += iter_start.elapsed();

            if trace && (iter_idx as usize) < trace_iters {
                eprintln!(
                    "[trace][{}][iter {}] setup(factory={:.2?}, genesis={:.2?}) exec(evm={:.2?}, write={:.2?}, root+commit={:.2?}, total={:.2?}) teardown(drop_factory={:.2?}) wall={:.2?}",
                    label,
                    iter_idx + 1,
                    setup,
                    genesis,
                    evm_phase,
                    write_phase,
                    root_phase,
                    evm_phase + write_phase + root_phase,
                    teardown,
                    iter_start.elapsed()
                );
            }
        }
        eprintln!(
            "[{}] avg/blk(block-lifecycle): {:.2?}",
            label,
            exec_total / (iters * block_txs.len() as u64) as u32
        );
        eprintln!(
            "[{}] avg/iter breakdown: setup={:.2?} (genesis={:.2?}) evm={:.2?} write={:.2?} root+commit={:.2?} teardown={:.2?}",
            label,
            setup_total / iters as u32,
            genesis_total / iters as u32,
            evm_total / iters as u32,
            write_total / iters as u32,
            root_total / iters as u32,
            teardown_total / iters as u32,
        );
        eprintln!("[{}] avg/iter(end-to-end): {:.2?}", label, wall_total / iters as u32);
        eprintln!("[{}] criterion measure mode: {:?}", label, measure_mode);
        if matches!(measure_mode, BenchMeasureMode::EndToEnd) {
            wall_total
        } else {
            exec_total
        }
    });
}

// ── Criterion ─────────────────────────────────────────────────────────────────

fn bench_eth_transfer(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let dataset = setup_mixed_state(&mut cache, &mut rng);
    let block_txs =
        generate_eth_block_txs(&dataset.eoa_addresses, &dataset.all_addresses, &cache, &mut rng);
    let id = format!(
        "{}acc_{}tx_{}blk_{}c_{}kv",
        pre_pop_accounts(),
        txs_per_block(),
        num_blocks(),
        (contract_ratio() * 100.0).round() as u32,
        contract_kv_per_contract()
    );
    let sample_size = bench_sample_size();
    let warmup_secs = bench_warmup_secs();
    let measurement_secs = bench_measurement_secs();

    let mut group = c.benchmark_group("eth_transfer");
    group.sample_size(sample_size);
    group.warm_up_time(std::time::Duration::from_secs(warmup_secs));
    group.measurement_time(std::time::Duration::from_secs(measurement_secs));

    group.bench_with_input(BenchmarkId::new("mptdb", &id), &(), |b, _| {
        run_mptdb_bench(b, &cache, &block_txs, "mptdb");
    });

    group.bench_with_input(BenchmarkId::new("reth_mdbx", &id), &(), |b, _| {
        run_reth_mdbx_bench(b, &cache, &block_txs, "reth_mdbx");
    });

    group.finish();
}

// ── ERC20 benchmark ───────────────────────────────────────────────────────────
// Minimal ERC-20 (transfer + balanceOf), solc 0.8.30, 393 bytes.
// Balances at slot keccak256(abi.encode(holder, 0)).
// ERC20 workload exercises storage-trie writes — the area where mptdb
// has its main architectural advantage over reth MDBX.

const FALLBACK_ERC20_CONTRACT: Address = Address::new([0xC0; 20]);

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

fn erc20_balance_slot(holder: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    U256::from_be_bytes(keccak256(buf).0)
}

fn encode_transfer(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // transfer(address,uint256)
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

fn setup_erc20(
    cache: &mut InMemoryCache,
    contract_addresses: &[Address],
    holder_addresses: &[Address],
) -> Address {
    let erc20_contract = contract_addresses.first().copied().unwrap_or(FALLBACK_ERC20_CONTRACT);
    cache.insert_contract(erc20_contract, Bytes::from_static(&ERC20_RUNTIME_BYTECODE));
    let slots = cache.storage.entry(erc20_contract).or_default();
    for &addr in holder_addresses {
        slots.insert(erc20_balance_slot(addr), U256::from(1_000_000u64));
    }
    erc20_contract
}

fn generate_erc20_block_txs(
    eoa_addresses: &[Address],
    erc20_contract: Address,
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let blocks_n = num_blocks();
    // Cap to available senders to prevent infinite loop (see generate_eth_block_txs).
    let txs_n = txs_per_block().min(eoa_addresses.len());
    let mut nonces: HashMap<Address, u64> = eoa_addresses
        .iter()
        .map(|&addr| (addr, cache.accounts.get(&addr).map(|i| i.nonce).unwrap_or(0)))
        .collect();
    let mut blocks = Vec::with_capacity(blocks_n);
    for _ in 0..blocks_n {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(txs_n);
        while txs.len() < txs_n {
            let sender_idx = rng.random_range(0..eoa_addresses.len());
            if !used.insert(sender_idx) {
                continue;
            }
            let sender = eoa_addresses[sender_idx];
            let receiver = eoa_addresses[rng.random_range(0..eoa_addresses.len())];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            txs.push(TxEnv {
                caller: sender,
                kind: TxKind::Call(erc20_contract),
                value: U256::ZERO,
                gas_limit: 100_000,
                gas_price: 0,
                nonce,
                chain_id: Some(1),
                data: encode_transfer(receiver, U256::from(100u64)),
                ..Default::default()
            });
            *nonces.entry(sender).or_insert(0) += 1;
        }
        blocks.push(txs);
    }
    blocks
}

fn select_active_erc20_contracts(contract_addresses: &[Address]) -> Vec<Address> {
    select_active_erc20_contracts_with_ratio(contract_addresses, ERC20_ACTIVE_CONTRACT_POOL_RATIO)
}

fn select_active_erc20_contracts_with_ratio(
    contract_addresses: &[Address],
    ratio: f64,
) -> Vec<Address> {
    if contract_addresses.is_empty() {
        return vec![FALLBACK_ERC20_CONTRACT];
    }
    let active_count = ((contract_addresses.len() as f64) * ratio.clamp(0.0, 1.0)).ceil() as usize;
    let active_count = active_count.max(1).min(contract_addresses.len());
    contract_addresses[..active_count].to_vec()
}

fn generate_erc20_block_txs_contract_pool(
    eoa_addresses: &[Address],
    contract_pool: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    generate_erc20_block_txs_contract_pool_with_config(
        eoa_addresses,
        contract_pool,
        cache,
        rng,
        num_blocks(),
        txs_per_block(),
        contract_kv_per_contract(),
    )
}

fn generate_erc20_block_txs_contract_pool_with_config(
    eoa_addresses: &[Address],
    contract_pool: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
    blocks_n: usize,
    txs_n: usize,
    kv_per_contract: usize,
) -> Vec<Vec<TxEnv>> {
    let mut nonces: HashMap<Address, u64> = eoa_addresses
        .iter()
        .map(|&addr| (addr, cache.accounts.get(&addr).map(|i| i.nonce).unwrap_or(0)))
        .collect();

    // setup_mixed_state() pre-fills each contract's ERC20-like holders with:
    // holder_idx = (contract_idx * kv_per_contract + j) % eoa_len
    let holders_by_contract: Vec<Vec<Address>> = (0..contract_pool.len())
        .map(|contract_idx| {
            (0..kv_per_contract)
                .map(|j| eoa_addresses[(contract_idx * kv_per_contract + j) % eoa_addresses.len()])
                .collect()
        })
        .collect();

    let mut blocks = Vec::with_capacity(blocks_n);
    for _ in 0..blocks_n {
        let mut txs = Vec::with_capacity(txs_n);
        for _ in 0..txs_n {
            let contract_idx = rng.random_range(0..contract_pool.len());
            let sender_holders = &holders_by_contract[contract_idx];
            let sender = sender_holders[rng.random_range(0..sender_holders.len())];
            let receiver = eoa_addresses[rng.random_range(0..eoa_addresses.len())];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            txs.push(TxEnv {
                caller: sender,
                kind: TxKind::Call(contract_pool[contract_idx]),
                value: U256::ZERO,
                gas_limit: 100_000,
                gas_price: 0,
                nonce,
                chain_id: Some(1),
                data: encode_transfer(receiver, U256::from(100u64)),
                ..Default::default()
            });
            *nonces.entry(sender).or_insert(0) += 1;
        }
        blocks.push(txs);
    }
    blocks
}

fn bench_erc20_transfer(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let dataset = setup_mixed_state(&mut cache, &mut rng);
    let erc20_contract =
        setup_erc20(&mut cache, &dataset.contract_addresses, &dataset.eoa_addresses);
    let block_txs =
        generate_erc20_block_txs(&dataset.eoa_addresses, erc20_contract, &cache, &mut rng);
    let id = format!(
        "{}acc_{}tx_{}blk_{}c_{}kv",
        pre_pop_accounts(),
        txs_per_block(),
        num_blocks(),
        (contract_ratio() * 100.0).round() as u32,
        contract_kv_per_contract()
    );
    let sample_size = bench_sample_size();
    let warmup_secs = bench_warmup_secs();
    let measurement_secs = bench_measurement_secs();

    let mut group = c.benchmark_group("erc20_transfer");
    group.sample_size(sample_size);
    group.warm_up_time(std::time::Duration::from_secs(warmup_secs));
    group.measurement_time(std::time::Duration::from_secs(measurement_secs));

    group.bench_with_input(BenchmarkId::new("mptdb", &id), &(), |b, _| {
        run_mptdb_bench(b, &cache, &block_txs, "mptdb/erc20");
    });

    group.bench_with_input(BenchmarkId::new("reth_mdbx", &id), &(), |b, _| {
        run_reth_mdbx_bench(b, &cache, &block_txs, "reth_mdbx/erc20");
    });

    group.finish();
}

fn bench_erc20_transfer_10pct_contract_pool(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let dataset = setup_mixed_state(&mut cache, &mut rng);
    let contract_pool = select_active_erc20_contracts(&dataset.contract_addresses);
    if dataset.contract_addresses.is_empty() {
        // Ensure fallback contract exists when contract_ratio=0.
        let _ = setup_erc20(&mut cache, &contract_pool, &dataset.eoa_addresses);
    }
    let block_txs = generate_erc20_block_txs_contract_pool(
        &dataset.eoa_addresses,
        &contract_pool,
        &cache,
        &mut rng,
    );

    let id = format!(
        "{}acc_{}tx_{}blk_{}c_{}kv_pool{}",
        pre_pop_accounts(),
        txs_per_block(),
        num_blocks(),
        (contract_ratio() * 100.0).round() as u32,
        contract_kv_per_contract(),
        contract_pool.len()
    );
    let sample_size = bench_sample_size();
    let warmup_secs = bench_warmup_secs();
    let measurement_secs = bench_measurement_secs();

    let mut group = c.benchmark_group("erc20_transfer_10pct_contract_pool");
    group.sample_size(sample_size);
    group.warm_up_time(std::time::Duration::from_secs(warmup_secs));
    group.measurement_time(std::time::Duration::from_secs(measurement_secs));

    group.bench_with_input(BenchmarkId::new("mptdb", &id), &(), |b, _| {
        run_mptdb_bench(b, &cache, &block_txs, "mptdb/erc20_pool10");
    });

    group.bench_with_input(BenchmarkId::new("reth_mdbx", &id), &(), |b, _| {
        run_reth_mdbx_bench(b, &cache, &block_txs, "reth_mdbx/erc20_pool10");
    });

    group.finish();
}

fn bench_b5_0_peak_integration_10x(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();

    let pre_pop = b5_0_pre_pop_accounts();
    let blocks = b5_0_num_blocks();
    let txs = b5_0_txs_per_block();
    let c_ratio = b5_0_contract_ratio();
    let kv_per_contract = b5_0_contract_kv_per_contract();
    let active_pool_ratio = b5_0_active_contract_pool_ratio();

    let dataset =
        setup_mixed_state_with_config(&mut cache, &mut rng, pre_pop, txs, c_ratio, kv_per_contract);
    let contract_pool =
        select_active_erc20_contracts_with_ratio(&dataset.contract_addresses, active_pool_ratio);
    if dataset.contract_addresses.is_empty() {
        let _ = setup_erc20(&mut cache, &contract_pool, &dataset.eoa_addresses);
    }
    let block_txs = generate_erc20_block_txs_contract_pool_with_config(
        &dataset.eoa_addresses,
        &contract_pool,
        &cache,
        &mut rng,
        blocks,
        txs,
        kv_per_contract,
    );

    let id = format!(
        "{}acc_{}tx_{}blk_{}c_{}kv_pool{}",
        pre_pop,
        txs,
        blocks,
        (c_ratio * 100.0).round() as u32,
        kv_per_contract,
        contract_pool.len()
    );
    let sample_size = bench_sample_size();
    let warmup_secs = bench_warmup_secs();
    let measurement_secs = bench_measurement_secs();

    let mut group = c.benchmark_group("b5_0_peak_integration_10x");
    group.sample_size(sample_size);
    group.warm_up_time(std::time::Duration::from_secs(warmup_secs));
    group.measurement_time(std::time::Duration::from_secs(measurement_secs));

    group.bench_with_input(BenchmarkId::new("mptdb", &id), &(), |b, _| {
        run_mptdb_bench(b, &cache, &block_txs, "mptdb/b5_0_pool10x");
    });

    group.bench_with_input(BenchmarkId::new("reth_mdbx", &id), &(), |b, _| {
        run_reth_mdbx_bench(b, &cache, &block_txs, "reth_mdbx/b5_0_pool10x");
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_eth_transfer,
    bench_erc20_transfer,
    bench_erc20_transfer_10pct_contract_pool,
    bench_b5_0_peak_integration_10x
);
criterion_main!(benches);
