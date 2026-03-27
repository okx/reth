//! Block execution benchmark: mptdb-provider vs reth native MDBX.
//!
//! Same EVM workload (InMemoryCache + revm), comparing:
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
use mptdb_provider::MptDbStateWriter;
use mptdb_sc::mpt::{MptCommitStore, MptConfig};
use mptdb_ss::factory::new_state_store;
use parking_lot::Mutex;
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::prelude::*;
use reth_ethereum_primitives::Receipt as EthReceipt;
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
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;

// ── Config ────────────────────────────────────────────────────────────────────
const PRE_POP_ACCOUNTS: usize = 100_000;
const NUM_BLOCKS: usize = 10;
const TXS_PER_BLOCK: usize = 20_000;
const INITIAL_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH
const DEFAULT_SAMPLE_SIZE: usize = 10;
const DEFAULT_WARMUP_SECS: u64 = 3;
const DEFAULT_MEASUREMENT_SECS: u64 = 120;

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

// ── InMemoryCache — EVM read backend (same for both mptdb and MDBX) ───────────

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

    fn apply_bundle(&mut self, bundle: &BundleState) {
        for (addr, account) in bundle.state() {
            let addr = Address::from(*addr);
            if account.was_destroyed() {
                self.accounts.remove(&addr);
                self.storage.remove(&addr);
                continue;
            }
            if let Some(info) = &account.info {
                self.accounts.insert(addr, info.clone());
            }
            for (slot, slot_info) in &account.storage {
                self.storage.entry(addr).or_default().insert(*slot, slot_info.present_value);
            }
        }
    }
}

impl revm::DatabaseRef for InMemoryCache {
    type Error = Infallible;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
        Ok(self.accounts.get(&address).cloned())
    }
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Infallible> {
        Ok(self.code_by_hash.get(&code_hash).cloned().unwrap_or_default())
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Infallible> {
        Ok(self.storage.get(&address).and_then(|s| s.get(&index)).copied().unwrap_or(U256::ZERO))
    }
    fn block_hash_ref(&self, _number: u64) -> Result<B256, Infallible> {
        Ok(B256::ZERO)
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
    cache: &InMemoryCache,
    txs: impl Iterator<Item = TxEnv>,
    block_number: u64,
) -> BundleState {
    let state_db = State::builder().with_database_ref(cache).with_bundle_update().build();
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

fn setup_accounts(cache: &mut InMemoryCache, rng: &mut StdRng) -> Vec<Address> {
    let mut addresses = Vec::with_capacity(PRE_POP_ACCOUNTS);
    let mut addr_buf = [0u8; 20];
    for _ in 0..PRE_POP_ACCOUNTS {
        rng.fill(&mut addr_buf);
        let addr = Address::from(addr_buf);
        cache.insert_account(addr, U256::from(INITIAL_BALANCE), 0);
        addresses.push(addr);
    }
    addresses
}

fn generate_eth_block_txs(
    addresses: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let mut nonces: HashMap<Address, u64> =
        cache.accounts.iter().map(|(&a, i)| (a, i.nonce)).collect();
    let mut blocks = Vec::with_capacity(NUM_BLOCKS);
    for _ in 0..NUM_BLOCKS {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        while txs.len() < TXS_PER_BLOCK {
            let sender_idx = rng.random_range(0..addresses.len());
            if !used.insert(sender_idx) {
                continue;
            }
            let sender = addresses[sender_idx];
            let receiver = addresses[rng.random_range(0..addresses.len())];
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
        let mut exec_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        for _ in 0..iters {
            let iter_start = Instant::now();
            {
                let iter_dir = TempDir::new().unwrap();

                // Open mptdb
                let mut sc_config = MptConfig::default();
                // Benchmark default: measure the wal_first commit path used by
                // mpt-db high-performance mode. Set MPTDB_BENCH_LEGACY_SC=1 to
                // force legacy (non-wal-first) commits for A/B comparison.
                if std::env::var_os("MPTDB_BENCH_LEGACY_SC").is_none() {
                    sc_config.wal_first_commit = true;
                }
                let sc = Arc::new(Mutex::new(
                    MptCommitStore::open_with_config(iter_dir.path(), false, sc_config)
                        .expect("open SC"),
                ));
                let ss_config = StateStoreConfig {
                    db_directory: iter_dir.path().join("ss").to_string_lossy().to_string(),
                    keep_last_version: true,
                    ..Default::default()
                };
                let ss = new_state_store(&ss_config, &iter_dir.path().to_string_lossy())
                    .expect("open SS");
                let writer = MptDbStateWriter::<EthReceipt>::new(ss, Arc::clone(&sc));

                // Pre-populate genesis
                let genesis = cache_to_bundle(cache);
                writer.pre_populate(&genesis, 0).expect("pre_populate");

                let mut evm_cache = cache.clone();

                let exec_start = Instant::now();
                for (blk_idx, txs) in block_txs.iter().enumerate() {
                    let bundle =
                        execute_block_evm(&evm_cache, txs.clone().into_iter(), blk_idx as u64 + 1);
                    writer
                        .write_state(
                            &make_outcome(bundle.clone(), blk_idx as u64 + 1),
                            OriginalValuesKnown::Yes,
                            reth_storage_api::StateWriteConfig::default(),
                        )
                        .expect("write_state");
                    evm_cache.apply_bundle(&bundle);
                }
                exec_total += exec_start.elapsed();
            }
            wall_total += iter_start.elapsed();
        }
        eprintln!(
            "[{}] avg/blk(exec-only): {:.2?}",
            label,
            exec_total / (iters * NUM_BLOCKS as u64) as u32
        );
        eprintln!("[{}] avg/iter(end-to-end): {:.2?}", label, wall_total / iters as u32);
        wall_total
    });
}

// ── reth MDBX backend ─────────────────────────────────────────────────────────

fn run_reth_mdbx_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    label: &str,
) {
    use reth_provider::{test_utils::create_test_provider_factory, DBProvider, StateWriter};
    use reth_storage_api::TrieWriter;
    use reth_trie::HashedPostState;
    use reth_trie_db::DatabaseStateRoot;

    b.iter_custom(|iters| {
        let mut exec_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        for _ in 0..iters {
            let iter_start = Instant::now();
            {
                let factory = create_test_provider_factory();

                // Pre-populate genesis state + compute genesis trie root
                let genesis = cache_to_bundle(cache);
                {
                    let provider = factory.provider_rw().expect("provider_rw");
                    provider
                        .write_state(
                            &make_outcome(genesis.clone(), 0),
                            OriginalValuesKnown::Yes,
                            reth_storage_api::StateWriteConfig::default(),
                        )
                        .expect("genesis write_state");
                    // Engine mode: compute state root using DatabaseStateRoot on MDBX tx
                    let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                        genesis.state.par_iter(),
                    );
                    let (_root, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                        provider.tx_ref(),
                        &hashed.into_sorted(),
                    )
                    .expect("genesis state_root");
                    provider.write_trie_updates(trie_updates).expect("trie_updates");
                    provider.commit().expect("genesis commit");
                }

                let mut evm_cache = cache.clone();

                let exec_start = Instant::now();
                for (blk_idx, txs) in block_txs.iter().enumerate() {
                    let bundle =
                        execute_block_evm(&evm_cache, txs.clone().into_iter(), blk_idx as u64 + 1);
                    let provider = factory.provider_rw().expect("provider_rw");

                    // 1. Persist execution output (PlainState + HashedState)
                    provider
                        .write_state(
                            &make_outcome(bundle.clone(), blk_idx as u64 + 1),
                            OriginalValuesKnown::Yes,
                            reth_storage_api::StateWriteConfig::default(),
                        )
                        .expect("write_state");

                    // 2. Engine mode: compute state root per block (same as reth engine validation
                    //    path)
                    let hashed = HashedPostState::from_bundle_state::<reth_trie::KeccakKeyHasher>(
                        bundle.state.par_iter(),
                    );
                    let (_root, trie_updates) = reth_trie::StateRoot::overlay_root_with_updates(
                        provider.tx_ref(),
                        &hashed.into_sorted(),
                    )
                    .expect("state_root_with_updates");

                    // 3. Write trie updates for next block's incremental root computation
                    provider.write_trie_updates(trie_updates).expect("write_trie_updates");
                    provider.commit().expect("commit");
                    evm_cache.apply_bundle(&bundle);
                }
                exec_total += exec_start.elapsed();
            }
            wall_total += iter_start.elapsed();
        }
        eprintln!(
            "[{}] avg/blk(exec-only): {:.2?}",
            label,
            exec_total / (iters * NUM_BLOCKS as u64) as u32
        );
        eprintln!("[{}] avg/iter(end-to-end): {:.2?}", label, wall_total / iters as u32);
        wall_total
    });
}

// ── Criterion ─────────────────────────────────────────────────────────────────

fn bench_eth_transfer(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let addresses = setup_accounts(&mut cache, &mut rng);
    let block_txs = generate_eth_block_txs(&addresses, &cache, &mut rng);
    let id = format!("{PRE_POP_ACCOUNTS}acc_{TXS_PER_BLOCK}tx_{NUM_BLOCKS}blk");
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

const ERC20_CONTRACT: Address = Address::new([0xC0; 20]);

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

fn setup_erc20(cache: &mut InMemoryCache, addresses: &[Address]) {
    cache.insert_contract(ERC20_CONTRACT, Bytes::from_static(&ERC20_RUNTIME_BYTECODE));
    let slots = cache.storage.entry(ERC20_CONTRACT).or_default();
    for &addr in addresses {
        slots.insert(erc20_balance_slot(addr), U256::from(1_000_000u64));
    }
}

fn generate_erc20_block_txs(
    addresses: &[Address],
    cache: &InMemoryCache,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let mut nonces: HashMap<Address, u64> =
        cache.accounts.iter().map(|(&a, i)| (a, i.nonce)).collect();
    let mut blocks = Vec::with_capacity(NUM_BLOCKS);
    for _ in 0..NUM_BLOCKS {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        while txs.len() < TXS_PER_BLOCK {
            let sender_idx = rng.random_range(0..addresses.len());
            if !used.insert(sender_idx) {
                continue;
            }
            let sender = addresses[sender_idx];
            let receiver = addresses[rng.random_range(0..addresses.len())];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            txs.push(TxEnv {
                caller: sender,
                kind: TxKind::Call(ERC20_CONTRACT),
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
    let addresses = setup_accounts(&mut cache, &mut rng);
    setup_erc20(&mut cache, &addresses);
    let block_txs = generate_erc20_block_txs(&addresses, &cache, &mut rng);
    let id = format!("{PRE_POP_ACCOUNTS}acc_{TXS_PER_BLOCK}tx_{NUM_BLOCKS}blk");
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

criterion_group!(benches, bench_eth_transfer, bench_erc20_transfer);
criterion_main!(benches);
