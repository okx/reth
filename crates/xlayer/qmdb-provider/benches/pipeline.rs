//! End-to-end pipeline benchmark: QmdbStateProvider → EVM execution → QmdbStateWriter.
//!
//! Measures full block processing using QMDB as the state backend via reth's
//! StateProvider/StateWriter traits, proving the integration works end-to-end.
//!
//! Two modes:
//! - **Sync**: flush after every block (latency-oriented)
//! - **Pipeline**: flush once after all blocks (throughput-oriented)
//!
//! Two workloads:
//! - **ETH transfers**: simple value transfers (21k gas each)
//! - **ERC20 transfers**: token transfers via contract call (~50k gas each)
//!
//! Run:
//!   cargo bench --bench pipeline -p xlayer-qmdb-provider
//!   cargo bench --bench pipeline -p xlayer-qmdb-provider -- "sync"
//!   cargo bench --bench pipeline -p xlayer-qmdb-provider -- "pipeline"
//!   cargo bench --bench pipeline -p xlayer-qmdb-provider -- "erc20"

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{rngs::StdRng, Rng, SeedableRng};
use revm::{
    context::{BlockEnv, Context, TxEnv},
    database::states::bundle_state::BundleRetention,
    handler::MainnetContext,
    primitives::hardfork::SpecId,
    ExecuteCommitEvm, MainBuilder,
};
use revm_database::{BundleState, State};
use revm_state::AccountInfo;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};
use xlayer_qmdb_provider::QmdbStore;

// -- Configuration --
const PRE_POP_ACCOUNTS: usize = 100_000;
const NUM_BLOCKS: usize = 10;
const TXS_PER_BLOCK: usize = 20_000;
const INITIAL_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH

// ---------------------------------------------------------------------------
// InMemoryStateProvider — HashMap-based cache for EVM reads
// ---------------------------------------------------------------------------

#[derive(Debug)]
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
                self.accounts.insert(
                    addr,
                    AccountInfo {
                        nonce: info.nonce,
                        balance: info.balance,
                        code_hash: info.code_hash,
                        code: info.code.clone(),
                        account_id: info.account_id,
                    },
                );
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

// ---------------------------------------------------------------------------
// QmdbDbRef — DatabaseRef backed by QmdbStore (for parallel pipeline benchmark)
// ---------------------------------------------------------------------------

/// DatabaseRef that reads from QMDB, with InMemoryCache fallback for bytecodes.
#[derive(Debug)]
struct QmdbDbRef<'a> {
    store: &'a QmdbStore,
    code_cache: &'a InMemoryCache,
}

impl revm::DatabaseRef for QmdbDbRef<'_> {
    type Error = Infallible;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
        // Try QMDB first, fall back to InMemoryCache (for contract code_hash)
        if let Some(acc) = self.store.read_account(&address) {
            // Check if InMemoryCache has code info for this address
            let (code_hash, code) = if let Some(mem_info) = self.code_cache.accounts.get(&address) {
                (mem_info.code_hash, mem_info.code.clone())
            } else {
                (KECCAK_EMPTY, None)
            };
            Ok(Some(AccountInfo {
                nonce: acc.nonce,
                balance: acc.balance,
                code_hash,
                code,
                account_id: None,
            }))
        } else {
            // Not in QMDB — check InMemoryCache (for contracts deployed in memory)
            Ok(self.code_cache.accounts.get(&address).cloned())
        }
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Infallible> {
        // QMDB doesn't store bytecodes — use InMemoryCache
        Ok(self.code_cache.code_by_hash.get(&code_hash).cloned().unwrap_or_default())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Infallible> {
        // Read from QMDB
        let slot = B256::new(index.to_be_bytes());
        Ok(self.store.read_storage(&address, &slot).unwrap_or(U256::ZERO))
    }

    fn block_hash_ref(&self, _number: u64) -> Result<B256, Infallible> {
        Ok(B256::ZERO)
    }
}

// ---------------------------------------------------------------------------
// Convert InMemoryCache to BundleState for pre-population
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// EVM execution
// ---------------------------------------------------------------------------

fn execute_block_evm(
    cache: &InMemoryCache,
    txs: impl Iterator<Item = TxEnv>,
    block_number: u64,
) -> (BundleState, Duration) {
    let evm_start = Instant::now();
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
    let bundle = state_db.take_bundle();
    (bundle, evm_start.elapsed())
}

// ---------------------------------------------------------------------------
// Transaction generation
// ---------------------------------------------------------------------------

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
    num_blocks: usize,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    for (&addr, info) in &cache.accounts {
        nonces.insert(addr, info.nonce);
    }
    let mut blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
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

// ---------------------------------------------------------------------------
// ERC20 workload
// ---------------------------------------------------------------------------

const ERC20_RUNTIME_BYTECODE: &[u8] = &hex_literal(
    "608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb1461006a575b5f5ffd5b610057610046366004610104565b5f6020819052908152604090205481565b6040519081526020015b60405180910390f35b61007d610078366004610124565b61008d565b6040519015158152602001610061565b335f908152602081905260408120805483919083906100ad908490610160565b90915550506001600160a01b0383165f90815260208190526040812080548492906100d9908490610173565b9091555060019150505b92915050565b80356001600160a01b03811681146100ff575f5ffd5b919050565b5f60208284031215610114575f5ffd5b61011d826100e9565b9392505050565b5f5f60408385031215610135575f5ffd5b61013e836100e9565b946020939093013593505050565b634e487b7160e01b5f52601160045260245ffd5b818103818111156100e3576100e361014c565b808201808211156100e3576100e361014c56fea26469706673582212202ab3fe360a062c91ad12f573bd1c234812f445f817ce8adbad19c108f60a822d64736f6c634300081e0033"
);

const fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("invalid hex char"),
    }
}

const fn hex_literal(hex: &str) -> [u8; 393] {
    let hex = hex.as_bytes();
    let mut out = [0u8; 393];
    let mut i = 0;
    while i < 393 {
        out[i] = hex_val(hex[i * 2]) << 4 | hex_val(hex[i * 2 + 1]);
        i += 1;
    }
    out
}

const ERC20_CONTRACT: Address = Address::new([0xC0; 20]);

fn erc20_balance_slot(holder: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    U256::from_be_bytes(keccak256(buf).0)
}

fn encode_transfer(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

fn setup_erc20(cache: &mut InMemoryCache, addresses: &[Address]) {
    cache.insert_contract(ERC20_CONTRACT, Bytes::from_static(ERC20_RUNTIME_BYTECODE));
    let contract_storage = cache.storage.entry(ERC20_CONTRACT).or_default();
    for &addr in addresses {
        contract_storage.insert(erc20_balance_slot(addr), U256::from(1_000_000u64));
    }
}

fn generate_erc20_block_txs(
    addresses: &[Address],
    num_blocks: usize,
    rng: &mut StdRng,
) -> Vec<Vec<TxEnv>> {
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    let transfer_amount = U256::from(100u64);
    let mut blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
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
                data: encode_transfer(receiver, transfer_amount),
                gas_limit: 100_000,
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

/// Generate ERC-20 block transactions with pre-computed CrwSets (matching fafo).
///
/// fafo pre-computes CrwSets for 99% of ERC-20 transfers: the storage write set
/// contains the sender's and receiver's balance slots in the contract. Only 1%
/// of transactions require EVM simulation (for warmup/validation).
fn generate_erc20_with_crw_sets(
    addresses: &[Address],
    num_blocks: usize,
    rng: &mut StdRng,
) -> Vec<Vec<(TxEnv, Option<xlayer_parallel_exec::crw_sets::CrwSets>)>> {
    use xlayer_parallel_exec::crw_sets::{short_hash_slot, CrwSets};

    let mut nonces: HashMap<Address, u64> = HashMap::new();
    let transfer_amount = U256::from(100u64);
    let mut blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        let mut tx_idx = 0usize;
        while txs.len() < TXS_PER_BLOCK {
            let sender_idx = rng.random_range(0..addresses.len());
            if !used.insert(sender_idx) {
                continue;
            }
            let sender = addresses[sender_idx];
            let receiver = addresses[rng.random_range(0..addresses.len())];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            let tx = TxEnv {
                caller: sender,
                kind: TxKind::Call(ERC20_CONTRACT),
                data: encode_transfer(receiver, transfer_amount),
                gas_limit: 100_000,
                gas_price: 0,
                nonce,
                chain_id: Some(1),
                ..Default::default()
            };

            // Match fafo: 99% of txs get pre-computed CrwSets, 1% need EVM simulation.
            // CrwSets contain the storage slots that the ERC-20 transfer writes:
            //   - balances[sender] (slot = keccak256(sender, 0))
            //   - balances[receiver] (slot = keccak256(receiver, 0))
            let crw_sets = if tx_idx % 100 != 0 {
                let sender_slot = erc20_balance_slot(sender);
                let receiver_slot = erc20_balance_slot(receiver);
                Some(CrwSets {
                    account_reads: vec![],
                    account_writes: vec![],
                    storage_reads: vec![],
                    storage_writes: vec![
                        short_hash_slot(&ERC20_CONTRACT, &sender_slot),
                        short_hash_slot(&ERC20_CONTRACT, &receiver_slot),
                    ],
                })
            } else {
                None // This 1% will go through full EVM simulation
            };

            txs.push((tx, crw_sets));
            *nonces.entry(sender).or_insert(0) += 1;
            tx_idx += 1;
        }
        blocks.push(txs);
    }
    blocks
}

// ---------------------------------------------------------------------------
// Stats reporting
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BlockStats {
    evm_time: Duration,
    commit_time: Duration,
    total_time: Duration,
    state_entries: usize,
}

fn print_stats(label: &str, stats: &[BlockStats]) {
    let n = stats.len();
    if n == 0 {
        return;
    }
    let avg = |f: fn(&BlockStats) -> Duration| -> Duration {
        stats.iter().map(f).sum::<Duration>() / n as u32
    };
    let total_time: Duration = stats.iter().map(|s| s.total_time).sum();
    let total_tx = n * TXS_PER_BLOCK;
    let tps = total_tx as f64 / total_time.as_secs_f64();
    let avg_entries = stats.iter().map(|s| s.state_entries).sum::<usize>() / n;
    eprintln!("─── {} ───", label);
    eprintln!(
        "  {} blocks, {} tx/block  |  avg block: {:.2?}  (evm {:.2?}  commit {:.2?})  entries: {}",
        n,
        TXS_PER_BLOCK,
        avg(|s| s.total_time),
        avg(|s| s.evm_time),
        avg(|s| s.commit_time),
        avg_entries,
    );
    eprintln!("  throughput: {:.0} tx/s", tps);
}

// ---------------------------------------------------------------------------
// Sync benchmark: QmdbStore commit (flush per block)
// ---------------------------------------------------------------------------

fn run_qmdb_sync_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    store: &Arc<QmdbStore>,
    label: &str,
) {
    b.iter_custom(|iters| {
        let mut total = Duration::ZERO;
        for i in 0..iters {
            let mut evm_cache = InMemoryCache {
                accounts: cache.accounts.clone(),
                storage: cache.storage.clone(),
                code_by_hash: cache.code_by_hash.clone(),
            };

            let mut block_stats = Vec::with_capacity(block_txs.len());
            let iter_start = Instant::now();

            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let block_start = Instant::now();

                // 1. EVM execution
                let (bundle, evm_time) =
                    execute_block_evm(&evm_cache, txs.clone().into_iter(), blk_idx as u64 + 1);

                let state_entries = bundle.state().len();

                // 2. Commit to QMDB (sync = flush per block)
                let commit_start = Instant::now();
                store.commit_bundle(&bundle);
                let commit_time = commit_start.elapsed();

                // 3. Update in-memory cache for next block
                evm_cache.apply_bundle(&bundle);

                block_stats.push(BlockStats {
                    evm_time,
                    commit_time,
                    total_time: block_start.elapsed(),
                    state_entries,
                });
            }

            total += iter_start.elapsed();
            if i == 0 {
                print_stats(label, &block_stats);
                let root = store.state_root();
                eprintln!("  state root: {root}");
            }
        }
        total
    });
}

// ---------------------------------------------------------------------------
// Pipeline benchmark: QmdbStore submit + single flush
// ---------------------------------------------------------------------------

fn run_qmdb_pipeline_bench(
    b: &mut criterion::Bencher<'_>,
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    store: &Arc<QmdbStore>,
    label: &str,
) {
    b.iter_custom(|iters| {
        let mut total = Duration::ZERO;
        for i in 0..iters {
            let mut evm_cache = InMemoryCache {
                accounts: cache.accounts.clone(),
                storage: cache.storage.clone(),
                code_by_hash: cache.code_by_hash.clone(),
            };

            let mut block_stats = Vec::with_capacity(block_txs.len());
            let mut total_submit = Duration::ZERO;
            let iter_start = Instant::now();

            for (blk_idx, txs) in block_txs.iter().enumerate() {
                let block_start = Instant::now();

                let (bundle, evm_time) =
                    execute_block_evm(&evm_cache, txs.clone().into_iter(), blk_idx as u64 + 1);
                let state_entries = bundle.state().len();

                let submit_start = Instant::now();
                store.submit_bundle(&bundle);
                total_submit += submit_start.elapsed();

                evm_cache.apply_bundle(&bundle);

                block_stats.push(BlockStats {
                    evm_time,
                    commit_time: submit_start.elapsed(),
                    total_time: block_start.elapsed(),
                    state_entries,
                });
            }

            let flush_start = Instant::now();
            store.flush();
            let flush_time = flush_start.elapsed();

            let total_elapsed = iter_start.elapsed();
            total += total_elapsed;

            if i == 0 {
                print_stats(label, &block_stats);
                let num = block_txs.len() as u32;
                eprintln!(
                    "  pipeline: avg submit {:.2?}  flush {:.2?}",
                    total_submit / num,
                    flush_time,
                );
                let root = store.state_root();
                eprintln!("  state root: {root}");
            }
        }
        total
    });
}

// ---------------------------------------------------------------------------
// Criterion benchmark groups
// ---------------------------------------------------------------------------

/// Helper: create a single QmdbStore, pre-populate it, and return (store, _dir).
/// The TempDir is returned to keep it alive for the store's lifetime.
fn create_prepopulated_store(cache: &InMemoryCache) -> (Arc<QmdbStore>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let store = QmdbStore::new(dir.path());
    let bundle = cache_to_bundle(cache);
    store.pre_populate(&bundle);
    (store, dir)
}

fn bench_qmdb_sync(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB provider sync");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let addresses = setup_accounts(&mut cache, &mut rng);

    // ETH transfers
    let mut rng_blocks = StdRng::seed_from_u64(43);
    let eth_blocks = generate_eth_block_txs(&addresses, &cache, NUM_BLOCKS, &mut rng_blocks);

    // ERC20 transfers
    setup_erc20(&mut cache, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_blocks = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    eprintln!("QMDB provider sync: {} accounts, {} tx/block", PRE_POP_ACCOUNTS, TXS_PER_BLOCK);

    // Create one QMDB instance per workload, reused across all iterations
    let (eth_store, _eth_dir) = create_prepopulated_store(&cache);
    group.bench_function(BenchmarkId::new("eth_sync", &label), |b| {
        run_qmdb_sync_bench(b, &cache, &eth_blocks, &eth_store, "QMDB Provider ETH (sync)");
    });

    let (erc20_store, _erc20_dir) = create_prepopulated_store(&cache);
    group.bench_function(BenchmarkId::new("erc20_sync", &label), |b| {
        run_qmdb_sync_bench(b, &cache, &erc20_blocks, &erc20_store, "QMDB Provider ERC20 (sync)");
    });

    group.finish();
}

fn bench_qmdb_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB provider pipeline");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let addresses = setup_accounts(&mut cache, &mut rng);

    let mut rng_blocks = StdRng::seed_from_u64(43);
    let eth_blocks = generate_eth_block_txs(&addresses, &cache, NUM_BLOCKS, &mut rng_blocks);

    setup_erc20(&mut cache, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_blocks = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    eprintln!("QMDB provider pipeline: {} accounts, {} tx/block", PRE_POP_ACCOUNTS, TXS_PER_BLOCK);

    let (eth_store, _eth_dir) = create_prepopulated_store(&cache);
    group.bench_function(BenchmarkId::new("eth_pipeline", &label), |b| {
        run_qmdb_pipeline_bench(b, &cache, &eth_blocks, &eth_store, "QMDB Provider ETH (pipeline)");
    });

    let (erc20_store, _erc20_dir) = create_prepopulated_store(&cache);
    group.bench_function(BenchmarkId::new("erc20_pipeline", &label), |b| {
        run_qmdb_pipeline_bench(
            b,
            &cache,
            &erc20_blocks,
            &erc20_store,
            "QMDB Provider ERC20 (pipeline)",
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Parallel pipeline benchmark: simulate → frame → execute (like fafo)
// ---------------------------------------------------------------------------

/// Run parallel pipeline benchmark with optional pre-computed CrwSets.
/// When `crw_blocks` is Some, uses pre-computed CrwSets (matching fafo).
/// When None, all txs go through full EVM simulation.
fn run_parallel_bench(
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    crw_blocks: Option<&[Vec<(TxEnv, Option<xlayer_parallel_exec::crw_sets::CrwSets>)>]>,
    qmdb_store: Option<&Arc<QmdbStore>>,
    label: &str,
) {
    use xlayer_parallel_exec::pipeline::{ParallelExecutionPipeline, PipelineTxInput};

    let mut pipeline = ParallelExecutionPipeline::with_config(16, 12, 64);

    let cfg_env = {
        let mut c = revm::context::CfgEnv::default();
        c.disable_nonce_check = true;
        c
    };

    let num_blocks = if crw_blocks.is_some() { crw_blocks.unwrap().len() } else { block_txs.len() };
    let total_txs = num_blocks * TXS_PER_BLOCK;

    // Warmup: run first block
    {
        let be = revm::context::BlockEnv {
            number: U256::from(0u64),
            gas_limit: u64::MAX,
            basefee: 0,
            ..Default::default()
        };
        let inputs: Vec<PipelineTxInput> = if let Some(crw) = crw_blocks {
            crw[0]
                .iter()
                .enumerate()
                .map(|(i, (tx, crw_sets))| PipelineTxInput {
                    sender: tx.caller,
                    tx_env: tx.clone(),
                    original_index: i,
                    pre_crw_sets: crw_sets.clone(),
                })
                .collect()
        } else {
            block_txs[0]
                .iter()
                .enumerate()
                .map(|(i, tx)| PipelineTxInput {
                    sender: tx.caller,
                    tx_env: tx.clone(),
                    original_index: i,
                    pre_crw_sets: None,
                })
                .collect()
        };
        if let Some(store) = qmdb_store {
            let db = QmdbDbRef { store, code_cache: cache };
            let _ = pipeline.execute_block(inputs, &db, &be, &cfg_env);
        } else {
            let _ = pipeline.execute_block(inputs, cache, &be, &cfg_env);
        }
    }

    // Measure
    let start = Instant::now();

    for blk_idx in 0..num_blocks {
        let be = revm::context::BlockEnv {
            number: U256::from(blk_idx as u64 + 1),
            gas_limit: u64::MAX,
            basefee: 0,
            ..Default::default()
        };

        let inputs: Vec<PipelineTxInput> = if let Some(crw) = crw_blocks {
            crw[blk_idx]
                .iter()
                .enumerate()
                .map(|(i, (tx, crw_sets))| PipelineTxInput {
                    sender: tx.caller,
                    tx_env: tx.clone(),
                    original_index: i,
                    pre_crw_sets: crw_sets.clone(),
                })
                .collect()
        } else {
            block_txs[blk_idx]
                .iter()
                .enumerate()
                .map(|(i, tx)| PipelineTxInput {
                    sender: tx.caller,
                    tx_env: tx.clone(),
                    original_index: i,
                    pre_crw_sets: None,
                })
                .collect()
        };

        if let Some(store) = qmdb_store {
            let db = QmdbDbRef { store, code_cache: cache };
            let result = pipeline.execute_block(inputs, &db, &be, &cfg_env);
            assert!(!result.tx_results.is_empty(), "block {} produced no results", blk_idx);
        } else {
            let result = pipeline.execute_block(inputs, cache, &be, &cfg_env);
            assert!(!result.tx_results.is_empty(), "block {} produced no results", blk_idx);
        }
    }

    let elapsed = start.elapsed();
    let tps = total_txs as f64 / elapsed.as_secs_f64();
    eprintln!("─── {} ───", label);
    eprintln!("  {} blocks, {} tx/block, {} total txs", num_blocks, TXS_PER_BLOCK, total_txs,);
    eprintln!("  elapsed: {:.2?}  |  {:.0} tx/s", elapsed, tps);
    eprintln!(
        "  avg block: {:.2?}  |  per-tx: {:.2?}",
        elapsed / num_blocks as u32,
        elapsed / total_txs as u32,
    );
}

fn run_serial_bench(
    cache: &InMemoryCache,
    block_txs: &[Vec<TxEnv>],
    qmdb_store: Option<&Arc<QmdbStore>>,
    label: &str,
) {
    let total_txs: usize = block_txs.iter().map(|b| b.len()).sum();

    let start = Instant::now();
    if let Some(store) = qmdb_store {
        // Serial with QMDB backend
        let qmdb_db = QmdbDbRef { store, code_cache: cache };
        for (blk_idx, txs) in block_txs.iter().enumerate() {
            let evm_start = Instant::now();
            let state_db =
                State::builder().with_database_ref(&qmdb_db).with_bundle_update().build();
            let block_env = revm::context::BlockEnv {
                number: U256::from(blk_idx as u64 + 1),
                gas_limit: u64::MAX,
                basefee: 0,
                ..Default::default()
            };
            let mut ctx: revm::handler::MainnetContext<_> =
                revm::context::Context::new(state_db, revm::primitives::hardfork::SpecId::CANCUN);
            ctx.cfg.disable_nonce_check = true;
            let ctx = ctx.with_block(block_env);
            let mut evm = ctx.build_mainnet();
            evm.transact_many_commit(txs.clone().into_iter()).expect("EVM execution failed");
            let state_db = &mut evm.ctx.journaled_state.database;
            state_db
                .merge_transitions(revm::database::states::bundle_state::BundleRetention::Reverts);
            let bundle = state_db.take_bundle();
            // Commit to QMDB
            store.commit_bundle(&bundle);
        }
    } else {
        // Serial with InMemoryCache
        let mut evm_cache = InMemoryCache {
            accounts: cache.accounts.clone(),
            storage: cache.storage.clone(),
            code_by_hash: cache.code_by_hash.clone(),
        };
        for (blk_idx, txs) in block_txs.iter().enumerate() {
            let (bundle, _) =
                execute_block_evm(&evm_cache, txs.clone().into_iter(), blk_idx as u64 + 1);
            evm_cache.apply_bundle(&bundle);
        }
    }
    let elapsed = start.elapsed();
    let tps = total_txs as f64 / elapsed.as_secs_f64();
    eprintln!("─── {} ───", label);
    eprintln!("  {} blocks, {} tx/block, {} total txs", block_txs.len(), TXS_PER_BLOCK, total_txs,);
    eprintln!("  elapsed: {:.2?}  |  {:.0} tx/s", elapsed, tps);
    eprintln!(
        "  avg block: {:.2?}  |  per-tx: {:.2?}",
        elapsed / block_txs.len() as u32,
        elapsed / total_txs as u32,
    );
}

fn bench_parallel_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("Parallel pipeline");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(120));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut cache = InMemoryCache::new();
    let addresses = setup_accounts(&mut cache, &mut rng);

    // ETH transfers
    let mut rng_eth = StdRng::seed_from_u64(43);
    let eth_blocks = generate_eth_block_txs(&addresses, &cache, NUM_BLOCKS, &mut rng_eth);

    // ERC20 transfers
    setup_erc20(&mut cache, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_blocks = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    // ERC20 with pre-computed CrwSets (matching fafo — 99% skip simulation)
    let mut rng_erc20_crw = StdRng::seed_from_u64(44); // same seed for comparable workload
    let erc20_crw_blocks = generate_erc20_with_crw_sets(&addresses, NUM_BLOCKS, &mut rng_erc20_crw);

    // --- Quick one-shot benchmarks (printed to stderr) ---
    eprintln!("\n========== Quick Benchmarks ==========");
    eprintln!(
        "Config: {} accounts, {} blocks, {} tx/block\n",
        PRE_POP_ACCOUNTS, NUM_BLOCKS, TXS_PER_BLOCK
    );

    // Create QMDB store for QMDB-backed benchmarks
    let (qmdb_store, _qmdb_dir) = create_prepopulated_store(&cache);

    // InMemory benchmarks (for reference)
    run_serial_bench(&cache, &eth_blocks, None, "Serial ETH (InMemory)");
    run_serial_bench(&cache, &erc20_blocks, None, "Serial ERC20 (InMemory)");
    run_parallel_bench(&cache, &eth_blocks, None, None, "Parallel ETH (InMemory)");
    run_parallel_bench(
        &cache,
        &erc20_blocks,
        Some(&erc20_crw_blocks),
        None,
        "Parallel ERC20 fafo-style (InMemory)",
    );

    // QMDB benchmarks (matches fafo's setup)
    run_serial_bench(&cache, &eth_blocks, Some(&qmdb_store), "Serial ETH (QMDB)");
    run_parallel_bench(&cache, &eth_blocks, None, Some(&qmdb_store), "Parallel ETH (QMDB)");
    run_serial_bench(&cache, &erc20_blocks, Some(&qmdb_store), "Serial ERC20 (QMDB)");
    run_parallel_bench(
        &cache,
        &erc20_blocks,
        Some(&erc20_crw_blocks),
        Some(&qmdb_store),
        "Parallel ERC20 fafo-style (QMDB)",
    );

    eprintln!("======================================\n");

    // --- Criterion-measured benchmarks ---

    // Parallel ERC20 with fafo-style pre-computed CrwSets
    group.bench_function(BenchmarkId::new("parallel_erc20_fafo", &label), |b| {
        use xlayer_parallel_exec::pipeline::{ParallelExecutionPipeline, PipelineTxInput};
        let mut pipeline = ParallelExecutionPipeline::with_config(16, 12, 64);
        let cfg_env = {
            let mut c = revm::context::CfgEnv::default();
            c.disable_nonce_check = true;
            c
        };
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                for (blk_idx, txs) in erc20_crw_blocks.iter().enumerate() {
                    let be = revm::context::BlockEnv {
                        number: U256::from(blk_idx as u64 + 1),
                        gas_limit: u64::MAX,
                        basefee: 0,
                        ..Default::default()
                    };
                    let inputs: Vec<PipelineTxInput> = txs
                        .iter()
                        .enumerate()
                        .map(|(i, (tx, crw_sets))| PipelineTxInput {
                            sender: tx.caller,
                            tx_env: tx.clone(),
                            original_index: i,
                            pre_crw_sets: crw_sets.clone(),
                        })
                        .collect();
                    let _ = pipeline.execute_block(inputs, &cache, &be, &cfg_env);
                }
                total += start.elapsed();
            }
            total
        });
    });

    // Parallel ERC20 without pre-computed CrwSets (full EVM simulation)
    group.bench_function(BenchmarkId::new("parallel_erc20_full_sim", &label), |b| {
        use xlayer_parallel_exec::pipeline::{ParallelExecutionPipeline, PipelineTxInput};
        let mut pipeline = ParallelExecutionPipeline::with_config(16, 12, 64);
        let cfg_env = {
            let mut c = revm::context::CfgEnv::default();
            c.disable_nonce_check = true;
            c
        };
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                for (blk_idx, txs) in erc20_blocks.iter().enumerate() {
                    let be = revm::context::BlockEnv {
                        number: U256::from(blk_idx as u64 + 1),
                        gas_limit: u64::MAX,
                        basefee: 0,
                        ..Default::default()
                    };
                    let inputs: Vec<PipelineTxInput> = txs
                        .iter()
                        .enumerate()
                        .map(|(i, tx)| PipelineTxInput {
                            sender: tx.caller,
                            tx_env: tx.clone(),
                            original_index: i,
                            pre_crw_sets: None,
                        })
                        .collect();
                    let _ = pipeline.execute_block(inputs, &cache, &be, &cfg_env);
                }
                total += start.elapsed();
            }
            total
        });
    });

    group.finish();
}

criterion_group!(benches, bench_qmdb_sync, bench_qmdb_pipeline, bench_parallel_pipeline);
criterion_main!(benches);
