//! Block execution benchmark: mptdb SC (state root) vs reth native MDBX.
//!
//! ## What this benchmark measures
//!
//! For each block:
//! - **mptdb lane**: default EVM reads from MDBX directly; set
//!   `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1` to route reads through `MptDbStateProvider`. SC
//!   commit (apply + WAL + state root) and MDBX PlainState write run in parallel.
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

fn bench_use_provider_reads() -> bool {
    std::env::var_os("MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS").is_some()
}

fn bench_enable_sc_prewarm() -> bool {
    std::env::var_os("MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM").is_some()
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
        let mut exec_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        let mut setup_total = Duration::ZERO;
        let mut teardown_total = Duration::ZERO;
        let mut evm_total = Duration::ZERO;
        let mut sc_write_total = Duration::ZERO;
        let mut write_total = Duration::ZERO; // wall of parallel SC+MDBX
        let mut prepop_total = Duration::ZERO;

        for iter_idx in 0..iters {
            let iter_start = Instant::now();
            let mut open_sc = Duration::ZERO;
            let mut pre_pop = Duration::ZERO;
            let mut evm_phase = Duration::ZERO;
            let mut sc_write_phase = Duration::ZERO;
            let mut write_phase = Duration::ZERO; // wall of parallel SC+MDBX
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
            let t = Instant::now();
            // Genesis → MDBX (PlainState + trie for initial root)
            {
                let provider = mdbx_factory.provider_rw().expect("genesis rw");
                provider
                    .write_state(
                        &make_outcome(genesis.clone(), 0),
                        OriginalValuesKnown::Yes,
                        bench_state_write_config(),
                    )
                    .expect("genesis mdbx write_state");
                let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                    genesis.state(),
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
            pre_pop += t.elapsed();

            // Pre-spawn one MDBX worker thread per iteration.
            // This avoids per-block thread-create overhead and removes the
            // worker from rayon's thread pool so SC's internal rayon tasks
            // don't contend with MDBX writes.
            //
            // Protocol per block:
            //   main → job_tx.send(Arc<Outcome>)   (non-blocking: bounded(1))
            //   main → SC commit (rayon, main thread)
            //   main → done_rx.recv()              (sync: MDBX done before next EVM)
            type JobOutcome = Arc<Outcome>;
            let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<JobOutcome>(1);
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(0);
            let mdbx_worker = {
                let factory = mdbx_factory.clone();
                std::thread::spawn(move || {
                    while let Ok(outcome) = job_rx.recv() {
                        let rw = factory.provider_rw().expect("mdbx rw");
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
                        rw.commit().expect("mdbx commit");
                        done_tx.send(()).expect("done signal");
                    }
                })
            };

            let exec_start = Instant::now();
            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let t_evm = Instant::now();
                let bundle = if use_provider_reads {
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
                job_tx.send(Arc::clone(&outcome)).expect("send to mdbx worker");

                // SC commit on main thread (uses rayon internally).
                let t_sc = Instant::now();
                sc_writer
                    .write_state(
                        outcome.as_ref(),
                        OriginalValuesKnown::Yes,
                        bench_state_write_config(),
                    )
                    .expect("sc write_state");
                sc_write_phase += t_sc.elapsed();

                // Wait for MDBX worker to finish before next block's EVM reads.
                done_rx.recv().expect("mdbx done");
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
            }
            exec_total += exec_start.elapsed();

            drop(job_tx); // signal MDBX worker to exit
            mdbx_worker.join().expect("mdbx worker");
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
            evm_total += evm_phase;
            sc_write_total += sc_write_phase;
            write_total += write_phase;
            teardown_total += drop_phase + tmp_drop;
            wall_total += iter_start.elapsed();

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
        eprintln!("[{}] avg/iter(end-to-end): {:.2?}", label, wall_total / iters as u32);
        eprintln!(
            "[{}] read_mode: provider_reads={} sc_prewarm={}",
            label, use_provider_reads, enable_sc_prewarm
        );
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
                        bench_state_write_config(),
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
