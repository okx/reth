//! Block execution benchmark: mptdb-provider vs reth native MDBX.
//!
//! Same EVM workload (revm + real StateProvider read path), comparing:
//! - **mptdb**: MptDbStateWriter (SC MPT + SS flat-KV)
//! - **reth-mdbx**: reth DatabaseProvider (MDBX B-tree)
//!
//! Run:
//!   cargo bench --bench block_execution -p mptdb-provider
//!   cargo bench --bench block_execution -p mptdb-provider -- "eth_transfer"

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use mptdb_common::config::StateStoreConfig;
use mptdb_provider::{MptDbStateProviderFactory, MptDbStateWriter};
use mptdb_sc::mpt::{MptCommitStore, MptConfig};
use mptdb_ss::factory::new_state_store;
use parking_lot::Mutex;
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::prelude::*;
use reth_ethereum_primitives::Receipt as EthReceipt;
use reth_storage_api::{
    errors::{
        db::DatabaseError,
        provider::{ProviderError, ProviderResult},
    },
    noop::NoopProvider,
    AccountReader, BlockHashReader, BlockIdReader, BlockNumReader, BytecodeReader,
    HashedPostStateProvider, StateProofProvider, StateProvider, StateProviderFactory,
    StateRootProvider, StorageRootProvider,
};
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, MultiProof, MultiProofTargets,
    StorageMultiProof, StorageProof, TrieInput,
};
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

fn bench_trace_enabled() -> bool {
    std::env::var_os("MPTDB_PROVIDER_BENCH_TRACE").is_some()
}

fn bench_trace_iters() -> usize {
    std::env::var("MPTDB_PROVIDER_BENCH_TRACE_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2)
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

#[derive(Clone)]
struct BytecodeFallbackProvider {
    noop: NoopProvider,
    code_by_hash: HashMap<B256, reth_primitives_traits::Bytecode>,
}

impl BytecodeFallbackProvider {
    fn from_cache(cache: &InMemoryCache) -> Self {
        let code_by_hash = cache
            .code_by_hash
            .iter()
            .map(|(hash, code)| (*hash, reth_primitives_traits::Bytecode(code.clone())))
            .collect();
        Self { noop: NoopProvider::default(), code_by_hash }
    }
}

impl AccountReader for BytecodeFallbackProvider {
    fn basic_account(
        &self,
        address: &Address,
    ) -> ProviderResult<Option<reth_primitives_traits::Account>> {
        self.noop.basic_account(address)
    }
}

impl BlockHashReader for BytecodeFallbackProvider {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.noop.block_hash(number)
    }

    fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
        self.noop.canonical_hashes_range(start, end)
    }
}

impl BlockNumReader for BytecodeFallbackProvider {
    fn chain_info(&self) -> ProviderResult<reth_chainspec::ChainInfo> {
        self.noop.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<u64> {
        self.noop.best_block_number()
    }

    fn last_block_number(&self) -> ProviderResult<u64> {
        self.noop.last_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<u64>> {
        self.noop.block_number(hash)
    }
}

impl BlockIdReader for BytecodeFallbackProvider {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        self.noop.pending_block_num_hash()
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        self.noop.safe_block_num_hash()
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        self.noop.finalized_block_num_hash()
    }
}

impl BytecodeReader for BytecodeFallbackProvider {
    fn bytecode_by_hash(
        &self,
        code_hash: &B256,
    ) -> ProviderResult<Option<reth_primitives_traits::Bytecode>> {
        Ok(self.code_by_hash.get(code_hash).cloned())
    }
}

impl StateRootProvider for BytecodeFallbackProvider {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support state_root".to_string(),
        )))
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support state_root_from_nodes".to_string(),
        )))
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support state_root_with_updates".to_string(),
        )))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support state_root_from_nodes_with_updates".to_string(),
        )))
    }
}

impl StorageRootProvider for BytecodeFallbackProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: reth_trie_common::HashedStorage,
    ) -> ProviderResult<B256> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support storage_root".to_string(),
        )))
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: reth_trie_common::HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support storage_proof".to_string(),
        )))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: reth_trie_common::HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support storage_multiproof".to_string(),
        )))
    }
}

impl StateProofProvider for BytecodeFallbackProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support proof".to_string(),
        )))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support multiproof".to_string(),
        )))
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Err(ProviderError::Database(DatabaseError::Other(
            "bytecode fallback does not support witness".to_string(),
        )))
    }
}

impl HashedPostStateProvider for BytecodeFallbackProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(bundle_state.state())
    }
}

impl StateProvider for BytecodeFallbackProvider {
    fn storage(&self, _account: Address, _storage_key: B256) -> ProviderResult<Option<U256>> {
        Ok(None)
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
    BundleState {
        state,
        contracts: Default::default(),
        reverts: Default::default(),
        state_size: 0,
        reverts_size: 0,
    }
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
    let total = pre_pop_accounts();
    let min_eoa = txs_per_block().max(1).min(total);
    let requested_contracts = ((total as f64) * contract_ratio()).round() as usize;
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
    let kv_per_contract = contract_kv_per_contract().min(eoa_addresses.len().max(1));
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
    let txs_n = txs_per_block();
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
    Outcome::new(bundle, Default::default(), block_number, Default::default())
}

// ── mptdb backend ─────────────────────────────────────────────────────────────

fn run_mptdb_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    label: &str,
) {
    use reth_storage_api::StateWriter;

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
        let mut prepop_total = Duration::ZERO;

        for iter_idx in 0..iters {
            let iter_start = Instant::now();
            let mut open_sc = Duration::ZERO;
            let mut open_ss = Duration::ZERO;
            let mut pre_pop = Duration::ZERO;
            let mut evm_phase = Duration::ZERO;
            let mut write_phase = Duration::ZERO;
            let mut drop_phase = Duration::ZERO;
            let mut tmp_drop = Duration::ZERO;

            let iter_dir = TempDir::new().unwrap();

            // Open mptdb
            let mut sc_config = MptConfig::default();
            // Benchmark default: measure the wal_first commit path used by
            // mpt-db high-performance mode. Set MPTDB_BENCH_LEGACY_SC=1 to
            // force legacy (non-wal-first) commits for A/B comparison.
            if std::env::var_os("MPTDB_BENCH_LEGACY_SC").is_none() {
                sc_config.wal_first_commit = true;
            }
            let t = Instant::now();
            let sc = Arc::new(Mutex::new(
                MptCommitStore::open_with_config(iter_dir.path(), false, sc_config).expect("open SC"),
            ));
            open_sc += t.elapsed();

            let writer = MptDbStateWriter::<EthReceipt>::new(Arc::clone(&sc));
            let fallback = Arc::new(BytecodeFallbackProvider::from_cache(cache));
            let block_id_reader = fallback.clone() as Arc<dyn BlockIdReader + Send + Sync>;
            let mpt_factory = MptDbStateProviderFactory::new(
                Arc::clone(&writer.sc),
                fallback as Arc<dyn StateProvider + Send + Sync>,
                block_id_reader,
            );

            // Pre-populate genesis
            let genesis = cache_to_bundle(cache);
            let t = Instant::now();
            writer.pre_populate(&genesis, 0).expect("pre_populate");
            pre_pop += t.elapsed();

            let exec_start = Instant::now();
            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let t_evm = Instant::now();
                let state_provider = mpt_factory.latest().expect("mpt latest state provider");
                let bundle =
                    execute_block_evm(state_provider, txs.clone().into_iter(), blk_idx as u64 + 1);
                evm_phase += t_evm.elapsed();

                let t_wr = Instant::now();
                writer
                    .write_state(
                        &make_outcome(bundle.clone(), blk_idx as u64 + 1),
                        OriginalValuesKnown::Yes,
                        reth_storage_api::StateWriteConfig::default(),
                    )
                    .expect("write_state");
                write_phase += t_wr.elapsed();
            }
            exec_total += exec_start.elapsed();

            let t = Instant::now();
            drop(writer);
            drop(sc);
            drop_phase += t.elapsed();
            let t = Instant::now();
            drop(iter_dir);
            tmp_drop += t.elapsed();

            setup_total += open_sc + open_ss + pre_pop;
            prepop_total += pre_pop;
            evm_total += evm_phase;
            write_total += write_phase;
            teardown_total += drop_phase + tmp_drop;
            wall_total += iter_start.elapsed();

            if trace && (iter_idx as usize) < trace_iters {
                eprintln!(
                    "[trace][{}][iter {}] setup(open_sc={:.2?}, open_ss={:.2?}, pre_pop={:.2?}) exec(evm={:.2?}, write={:.2?}, total={:.2?}) teardown(drop={:.2?}, tmp_drop={:.2?}) wall={:.2?}",
                    label,
                    iter_idx + 1,
                    open_sc,
                    open_ss,
                    pre_pop,
                    evm_phase,
                    write_phase,
                    evm_phase + write_phase,
                    drop_phase,
                    tmp_drop,
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
            "[{}] avg/iter breakdown: setup={:.2?} (pre_pop={:.2?}) evm={:.2?} write={:.2?} teardown={:.2?}",
            label,
            setup_total / iters as u32,
            prepop_total / iters as u32,
            evm_total / iters as u32,
            write_total / iters as u32,
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
            let genesis_bundle = cache_to_bundle(cache);
            {
                let t = Instant::now();
                let provider = factory.provider_rw().expect("provider_rw");
                provider
                    .write_state(
                        &make_outcome(genesis_bundle.clone(), 0),
                        OriginalValuesKnown::Yes,
                        reth_storage_api::StateWriteConfig::default(),
                    )
                    .expect("genesis write_state");
                // Engine mode: compute state root using DatabaseStateRoot on MDBX tx
                let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                    genesis_bundle.state.par_iter(),
                );
                let (_root, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                    provider.tx_ref(),
                    &hashed.into_sorted(),
                )
                .expect("genesis state_root");
                provider.write_trie_updates(trie_updates).expect("trie_updates");
                provider.commit().expect("genesis commit");
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

                // 1. Persist execution output (PlainState + HashedState)
                let t_wr = Instant::now();
                provider
                    .write_state(
                        &make_outcome(bundle.clone(), blk_idx as u64 + 1),
                        OriginalValuesKnown::Yes,
                        reth_storage_api::StateWriteConfig::default(),
                    )
                    .expect("write_state");
                write_phase += t_wr.elapsed();

                // 2. Engine mode: compute state root per block
                let t_root = Instant::now();
                let hashed =
                    HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                        bundle.state.par_iter(),
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
    let txs_n = txs_per_block();
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
    if contract_addresses.is_empty() {
        return vec![FALLBACK_ERC20_CONTRACT];
    }
    let active_count =
        ((contract_addresses.len() as f64) * ERC20_ACTIVE_CONTRACT_POOL_RATIO).ceil() as usize;
    let active_count = active_count.max(1).min(contract_addresses.len());
    contract_addresses[..active_count].to_vec()
}

fn generate_erc20_block_txs_contract_pool(
    eoa_addresses: &[Address],
    contract_pool: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let blocks_n = num_blocks();
    let txs_n = txs_per_block();
    let kv_per_contract = contract_kv_per_contract();

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

criterion_group!(
    benches,
    bench_eth_transfer,
    bench_erc20_transfer,
    bench_erc20_transfer_10pct_contract_pool
);
criterion_main!(benches);
