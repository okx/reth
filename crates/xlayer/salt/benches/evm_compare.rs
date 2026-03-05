//! End-to-end EVM benchmark: Transactions → EVM execution → BundleState → State commitment.
//!
//! Measures full block processing pipeline for SALT (AsyncRocksStore) and QMDB backends.
//! Two workloads: simple ETH transfers and ERC20 token transfers (2000 tx/block).
//!
//! Run:    cargo bench --bench evm_compare -p xlayer-salt
//! SALT:   cargo bench --bench evm_compare -p xlayer-salt -- "SALT"
//! QMDB:   cargo bench --bench evm_compare -p xlayer-salt -- "QMDB"
//! ERC20:  cargo bench --bench evm_compare -p xlayer-salt -- "erc20"

#![allow(missing_docs, unreachable_pub)]

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use parking_lot::RwLock;
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::{prelude::*, ThreadPoolBuilder};
use revm::{
    context::{BlockEnv, Context, TxEnv},
    database::states::bundle_state::BundleRetention,
    handler::MainnetContext,
    primitives::hardfork::SpecId,
    ExecuteCommitEvm, MainBuilder,
};
use revm_database::{BundleState, State};
use revm_state::AccountInfo;
use salt::{EphemeralSaltState, StateRoot as SaltStateRoot};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};
use xlayer_salt::{async_rocks_store::AsyncRocksStore, convert::bundle_state_to_plain_kv};

// -- Configuration --
const PRE_POP_ACCOUNTS: usize = 1_000_000;
const NUM_BLOCKS: usize = 10;
const TXS_PER_BLOCK: usize = 2000;
const SALT_NUM_THREADS: usize = 32;
/// 1 ETH in wei
const INITIAL_BALANCE: u128 = 1_000_000_000_000_000_000;

// ---------------------------------------------------------------------------
// InMemoryStateProvider — HashMap-based state for EVM reads
// ---------------------------------------------------------------------------

struct InMemoryStateProvider {
    accounts: HashMap<Address, AccountInfo>,
    storage: HashMap<Address, HashMap<U256, U256>>,
    code_by_hash: HashMap<B256, revm::bytecode::Bytecode>,
}

impl InMemoryStateProvider {
    fn new() -> Self {
        Self { accounts: HashMap::new(), storage: HashMap::new(), code_by_hash: HashMap::new() }
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

    fn insert_account(&mut self, addr: Address, balance: U256, nonce: u64) {
        self.accounts.insert(
            addr,
            AccountInfo { nonce, balance, code_hash: KECCAK_EMPTY, code: None, account_id: None },
        );
    }

    /// Apply BundleState changes to the in-memory provider for the next block.
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

impl revm::DatabaseRef for InMemoryStateProvider {
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
// ERC20 constants and helpers
// ---------------------------------------------------------------------------

/// Minimal ERC20 runtime bytecode (compiled with solc --optimize).
/// Supports `balanceOf(address)` and `transfer(address,uint256)`.
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

/// Fixed ERC20 contract address (must not collide with precompiles 0x01-0x09).
const ERC20_CONTRACT: Address = Address::new([0xC0; 20]);

/// ERC20 `balanceOf` mapping slot key: `keccak256(abi.encode(address, 0))`.
fn erc20_balance_slot(holder: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    // buf[32..64] stays 0 → slot 0 for balanceOf mapping
    U256::from_be_bytes(keccak256(buf).0)
}

/// ABI-encode `transfer(address,uint256)` calldata.
fn encode_transfer(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // selector
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

// ---------------------------------------------------------------------------
// EVM execution
// ---------------------------------------------------------------------------

/// Execute a block of transactions using revm.
/// Returns (BundleState, evm_duration).
fn execute_block_evm(
    provider: &InMemoryStateProvider,
    txs: impl Iterator<Item = TxEnv>,
    block_number: u64,
) -> (BundleState, Duration) {
    let evm_start = Instant::now();

    let state_db = State::builder().with_database_ref(provider).with_bundle_update().build();

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
    let evm_time = evm_start.elapsed();

    (bundle, evm_time)
}

// ---------------------------------------------------------------------------
// Transaction generation
// ---------------------------------------------------------------------------

struct BlockTxs {
    /// (sender, receiver, value, sender_nonce)
    txs: Vec<(Address, Address, U256, u64)>,
}

impl BlockTxs {
    /// Convert to ETH transfer TxEnvs.
    fn to_eth_tx_envs(&self) -> Vec<TxEnv> {
        self.txs
            .iter()
            .map(|(sender, receiver, value, nonce)| TxEnv {
                caller: *sender,
                kind: TxKind::Call(*receiver),
                value: *value,
                gas_limit: 21_000,
                gas_price: 0,
                nonce: *nonce,
                chain_id: Some(1),
                ..Default::default()
            })
            .collect()
    }
}

/// Generic block transactions (pre-built TxEnvs for any workload).
struct GenericBlockTxs {
    txs: Vec<TxEnv>,
}

/// Pre-populate provider with accounts and return their addresses.
fn setup_accounts(provider: &mut InMemoryStateProvider, rng: &mut StdRng) -> Vec<Address> {
    let mut addresses = Vec::with_capacity(PRE_POP_ACCOUNTS);
    let mut addr_buf = [0u8; 20];
    for _ in 0..PRE_POP_ACCOUNTS {
        rng.fill(&mut addr_buf);
        let addr = Address::from(addr_buf);
        provider.insert_account(addr, U256::from(INITIAL_BALANCE), 0);
        addresses.push(addr);
    }
    addresses
}

/// Generate transaction lists for each block. Picks unique senders to avoid nonce conflicts.
fn generate_block_txs(
    addresses: &[Address],
    provider: &InMemoryStateProvider,
    num_blocks: usize,
    rng: &mut StdRng,
) -> Vec<BlockTxs> {
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    for (&addr, info) in &provider.accounts {
        nonces.insert(addr, info.nonce);
    }

    let mut blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        let mut used = std::collections::HashSet::new();
        let mut txs = Vec::with_capacity(TXS_PER_BLOCK);
        while txs.len() < TXS_PER_BLOCK {
            let sender_idx = rng.random_range(0..addresses.len());
            if !used.insert(sender_idx) {
                continue; // skip duplicate sender within same block
            }
            let sender = addresses[sender_idx];
            let receiver_idx = rng.random_range(0..addresses.len());
            let receiver = addresses[receiver_idx];
            let nonce = nonces.get(&sender).copied().unwrap_or(0);
            txs.push((sender, receiver, U256::from(1u64), nonce));
            *nonces.entry(sender).or_insert(0) += 1;
        }
        blocks.push(BlockTxs { txs });
    }
    blocks
}

// ---------------------------------------------------------------------------
// ERC20 setup and transaction generation
// ---------------------------------------------------------------------------

/// Deploy ERC20 contract and pre-populate balances for all addresses.
fn setup_erc20(provider: &mut InMemoryStateProvider, addresses: &[Address]) {
    provider.insert_contract(ERC20_CONTRACT, Bytes::from_static(ERC20_RUNTIME_BYTECODE));
    let contract_storage = provider.storage.entry(ERC20_CONTRACT).or_default();
    for &addr in addresses {
        let slot = erc20_balance_slot(addr);
        contract_storage.insert(slot, U256::from(1_000_000u64));
    }
}

/// Generate ERC20 transfer TxEnvs for each block.
fn generate_erc20_block_txs(
    addresses: &[Address],
    num_blocks: usize,
    rng: &mut StdRng,
) -> Vec<GenericBlockTxs> {
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
            let receiver_idx = rng.random_range(0..addresses.len());
            let receiver = addresses[receiver_idx];
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
        blocks.push(GenericBlockTxs { txs });
    }
    blocks
}

// ---------------------------------------------------------------------------
// Per-block stats
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EvmBlockStats {
    evm_time: Duration,
    prep_time: Duration,
    delta_time: Duration,
    root_time: Duration,
    io_time: Duration,
    total_time: Duration,
    state_entries: usize,
    trie_entries: usize,
}

fn print_evm_stats(label: &str, stats: &[EvmBlockStats]) {
    let n = stats.len();
    if n == 0 {
        return;
    }
    let avg = |f: fn(&EvmBlockStats) -> Duration| -> Duration {
        stats.iter().map(f).sum::<Duration>() / n as u32
    };
    let avg_usize = |f: fn(&EvmBlockStats) -> usize| -> f64 {
        stats.iter().map(f).sum::<usize>() as f64 / n as f64
    };
    eprintln!("─── {} ───", label);
    eprintln!(
        "  {} blocks avg: {:.2?}  (evm {:.2?}  prep {:.2?}  delta {:.2?}  root {:.2?}  io {:.2?})",
        n,
        avg(|s| s.total_time),
        avg(|s| s.evm_time),
        avg(|s| s.prep_time),
        avg(|s| s.delta_time),
        avg(|s| s.root_time),
        avg(|s| s.io_time),
    );
    eprintln!(
        "  writes/blk: state {:.0}, trie {:.0}",
        avg_usize(|s| s.state_entries),
        avg_usize(|s| s.trie_entries),
    );
    eprintln!();
}

// ---------------------------------------------------------------------------
// SALT pre-populate helper (using BundleState from provider)
// ---------------------------------------------------------------------------

fn provider_to_bundle(provider: &InMemoryStateProvider) -> BundleState {
    use alloy_primitives::map::HashMap as PrimitivesHashMap;
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};

    let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
        PrimitivesHashMap::default();

    for (&addr, info) in &provider.accounts {
        let mut storage = StorageWithOriginalValues::default();
        if let Some(slots) = provider.storage.get(&addr) {
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

fn salt_pre_populate(
    store: &AsyncRocksStore,
    provider: &InMemoryStateProvider,
    pool: &rayon::ThreadPool,
) {
    let bundle = provider_to_bundle(provider);
    let kvs = bundle_state_to_plain_kv(&bundle);
    let mut eph = EphemeralSaltState::new(store);
    let state_updates = eph.update_fin(&kvs).unwrap();
    let mut root = SaltStateRoot::new(store).with_min_par_batch_size(4).with_deferred_levels(2);
    let (_root_hash, trie_updates) = pool.install(|| root.update_fin(&state_updates).unwrap());
    store.update_state_and_trie(state_updates, trie_updates).unwrap();
    store.wait_for_idle();
}

// ---------------------------------------------------------------------------
// SALT EVM benchmark (AsyncRocksStore)
// ---------------------------------------------------------------------------

/// Run the SALT EVM benchmark loop for a given set of per-block TxEnvs.
fn run_salt_evm_benchmark(
    b: &mut criterion::Bencher<'_>,
    provider: &InMemoryStateProvider,
    block_txs: &[GenericBlockTxs],
    pool: &rayon::ThreadPool,
    stats_label: &str,
) {
    b.iter_custom(|iters| {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AsyncRocksStore::new(dir.path()).unwrap();
        salt_pre_populate(&store, provider, pool);
        let snap = store.snapshot();

        let mut total = Duration::ZERO;
        for i in 0..iters {
            store.restore(&snap);
            let mut evm_provider = InMemoryStateProvider {
                accounts: provider.accounts.clone(),
                storage: provider.storage.clone(),
                code_by_hash: provider.code_by_hash.clone(),
            };
            let mut root =
                SaltStateRoot::new(&store).with_min_par_batch_size(4).with_deferred_levels(2);

            let mut block_stats = Vec::with_capacity(block_txs.len());
            let block_start_total = Instant::now();

            for (blk_idx, blk) in block_txs.iter().enumerate() {
                let block_start = Instant::now();

                // EVM execution
                let (bundle, evm_time) = execute_block_evm(
                    &evm_provider,
                    blk.txs.clone().into_iter(),
                    blk_idx as u64 + 1,
                );

                // Prep: BundleState → plain KVs
                let prep_start = Instant::now();
                let kvs = bundle_state_to_plain_kv(&bundle);
                let prep_time = prep_start.elapsed();

                // Parallel delta
                let delta_start = Instant::now();
                let state_updates = {
                    let session = store.read_session();
                    let mut groups: std::collections::HashMap<
                        u32,
                        Vec<(&Vec<u8>, &Option<Vec<u8>>)>,
                    > = std::collections::HashMap::new();
                    for (key, val) in &kvs {
                        let bid = salt::hasher::bucket_id(key);
                        groups.entry(bid).or_default().push((key, val));
                    }
                    let num_partitions = pool.current_num_threads().max(1);
                    let mut partitions: Vec<Vec<(&Vec<u8>, &Option<Vec<u8>>)>> =
                        (0..num_partitions).map(|_| Vec::new()).collect();
                    for (i, (_, group_kvs)) in groups.into_iter().enumerate() {
                        partitions[i % num_partitions].extend(group_kvs);
                    }
                    let results: Vec<salt::StateUpdates> = pool.install(|| {
                        partitions
                            .into_par_iter()
                            .filter_map(|partition| {
                                if partition.is_empty() {
                                    return None;
                                }
                                let mut eph = EphemeralSaltState::new(&session);
                                Some(eph.update_fin(partition.into_iter()).unwrap())
                            })
                            .collect()
                    });
                    let mut merged = salt::StateUpdates::default();
                    for updates in results {
                        merged.data.extend(updates.data);
                    }
                    Arc::new(merged)
                };
                let delta_time = delta_start.elapsed();

                // Dispatch state to bg writer
                store.dispatch_state_to_bg(Arc::clone(&state_updates)).unwrap();

                // Root + in-memory state update (overlapped)
                let root_start = Instant::now();
                let ((_root_hash, trie_updates), ws) = pool.install(|| {
                    rayon::join(
                        || root.update_fin(&state_updates).unwrap(),
                        || store.apply_state_in_memory(&state_updates),
                    )
                });
                let root_time = root_start.elapsed();

                // Trie dispatch
                let trie_start = Instant::now();
                let trie_entries = store.update_trie(trie_updates).unwrap();
                let trie_dispatch_time = trie_start.elapsed();

                // Update provider for next block
                evm_provider.apply_bundle(&bundle);

                block_stats.push(EvmBlockStats {
                    evm_time,
                    prep_time,
                    delta_time,
                    root_time,
                    io_time: ws.persist_duration + trie_dispatch_time,
                    total_time: block_start.elapsed(),
                    state_entries: ws.entries,
                    trie_entries,
                });
            }

            total += block_start_total.elapsed();
            if i == 0 {
                print_evm_stats(stats_label, &block_stats);
            }
        }
        total
    });
}

fn bench_salt_evm(c: &mut Criterion) {
    let mut group = c.benchmark_group("SALT evm");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let pool = ThreadPoolBuilder::new().num_threads(SALT_NUM_THREADS).build().unwrap();
    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut provider = InMemoryStateProvider::new();
    let addresses = setup_accounts(&mut provider, &mut rng);

    // ETH transfer workload
    let mut rng_blocks = StdRng::seed_from_u64(43);
    let eth_block_txs: Vec<GenericBlockTxs> =
        generate_block_txs(&addresses, &provider, NUM_BLOCKS, &mut rng_blocks)
            .into_iter()
            .map(|b| GenericBlockTxs { txs: b.to_eth_tx_envs() })
            .collect();

    // ERC20 transfer workload
    setup_erc20(&mut provider, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_block_txs = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    eprintln!("SALT setup: {} accounts, {} txs/block", PRE_POP_ACCOUNTS, TXS_PER_BLOCK);

    group.bench_function(BenchmarkId::new("async_rocks", &label), |b| {
        run_salt_evm_benchmark(
            b,
            &provider,
            &eth_block_txs,
            &pool,
            &format!("SALT AsyncRocks ETH ({SALT_NUM_THREADS}t)"),
        );
    });

    group.bench_function(BenchmarkId::new("erc20_async_rocks", &label), |b| {
        run_salt_evm_benchmark(
            b,
            &provider,
            &erc20_block_txs,
            &pool,
            &format!("SALT AsyncRocks ERC20 ({SALT_NUM_THREADS}t)"),
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// QMDB helpers (adapted from store_compare.rs)
// ---------------------------------------------------------------------------

use qmdb::{
    config::Config as QmdbConfig,
    def::{IN_BLOCK_IDX_BITS, OP_CREATE, OP_WRITE},
    seqads::task::{SingleCsTask, TaskBuilder},
    tasks::TasksManager,
    AdsCore, AdsWrap, ADS,
};
use reth_primitives_traits::Account;
use xlayer_salt::account::{
    account_plain_key, encode_account, encode_storage_value, storage_plain_key,
};

fn bundle_state_to_qmdb_task(bundle: &BundleState, op_type: u8) -> (SingleCsTask, usize) {
    let mut builder = TaskBuilder::new();
    let mut count = 0usize;

    for (address, bundle_account) in bundle.state() {
        let address = Address::from(*address);

        if let Some(info) = &bundle_account.info {
            let code_hash =
                if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
            let account =
                Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
            let key = account_plain_key(&address);
            let value = encode_account(&account);
            builder.add_op(op_type, &key, &value);
            count += 1;
        }

        for (slot, slot_info) in &bundle_account.storage {
            let slot_b256 = B256::from(*slot);
            let key = storage_plain_key(&address, &slot_b256);
            let value = encode_storage_value(&slot_info.present_value);
            builder.add_op(op_type, &key, &value);
            count += 1;
        }
    }

    (builder.build(), count)
}

const QMDB_PRE_POP_CHUNK_SIZE: usize = 20_000;

fn qmdb_pre_populate(ads: &mut AdsWrap<SingleCsTask>, provider: &InMemoryStateProvider) -> i64 {
    let bundle = provider_to_bundle(provider);
    let accounts: Vec<_> = bundle.state().iter().collect();
    let num_chunks = (accounts.len() + QMDB_PRE_POP_CHUNK_SIZE - 1) / QMDB_PRE_POP_CHUNK_SIZE;

    for (chunk_idx, chunk) in accounts.chunks(QMDB_PRE_POP_CHUNK_SIZE).enumerate() {
        let height = (chunk_idx + 1) as i64;
        let mut builder = TaskBuilder::new();

        for (address, bundle_account) in chunk {
            let address = Address::from(**address);
            if let Some(info) = &bundle_account.info {
                let code_hash =
                    if info.code_hash == KECCAK_EMPTY { None } else { Some(info.code_hash) };
                let account =
                    Account { nonce: info.nonce, balance: info.balance, bytecode_hash: code_hash };
                let key = account_plain_key(&address);
                let value = encode_account(&account);
                builder.add_op(OP_CREATE, &key, &value);
            }
            for (slot, slot_info) in &bundle_account.storage {
                let slot_b256 = B256::from(*slot);
                let key = storage_plain_key(&address, &slot_b256);
                let value = encode_storage_value(&slot_info.present_value);
                builder.add_op(OP_CREATE, &key, &value);
            }
        }

        let task = builder.build();
        let task_id: i64 = height << IN_BLOCK_IDX_BITS;
        let tasks_manager = Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
        ads.start_block(height, tasks_manager);
        let shared = ads.get_shared();
        shared.insert_extra_data(height, String::new());
        shared.add_task(task_id);
    }

    ads.flush();
    num_chunks as i64
}

// ---------------------------------------------------------------------------
// QMDB EVM sync benchmark helper
// ---------------------------------------------------------------------------

fn run_qmdb_sync_benchmark(
    b: &mut criterion::Bencher<'_>,
    provider: &InMemoryStateProvider,
    block_txs: &[GenericBlockTxs],
    ads: &mut AdsWrap<SingleCsTask>,
    next_height: &mut i64,
    stats_label: &str,
) {
    b.iter_custom(|iters| {
        let mut total = Duration::ZERO;
        for i in 0..iters {
            let mut evm_provider = InMemoryStateProvider {
                accounts: provider.accounts.clone(),
                storage: provider.storage.clone(),
                code_by_hash: provider.code_by_hash.clone(),
            };
            let mut block_stats = Vec::with_capacity(block_txs.len());
            let iter_start = Instant::now();

            for (blk_idx, blk) in block_txs.iter().enumerate() {
                let height = *next_height + blk_idx as i64;
                let block_start = Instant::now();

                // EVM execution
                let (bundle, evm_time) = execute_block_evm(
                    &evm_provider,
                    blk.txs.clone().into_iter(),
                    blk_idx as u64 + 1,
                );

                // Prep
                let prep_start = Instant::now();
                let (task, state_entries) = bundle_state_to_qmdb_task(&bundle, OP_WRITE);
                let prep_time = prep_start.elapsed();

                // Submit
                let submit_start = Instant::now();
                let task_id: i64 = height << IN_BLOCK_IDX_BITS;
                let tasks_manager =
                    Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
                ads.start_block(height, tasks_manager);
                let shared = ads.get_shared();
                shared.insert_extra_data(height, String::new());
                shared.add_task(task_id);
                let submit_time = submit_start.elapsed();

                // Flush (sync)
                let flush_start = Instant::now();
                ads.flush();
                let flush_time = flush_start.elapsed();

                evm_provider.apply_bundle(&bundle);

                block_stats.push(EvmBlockStats {
                    evm_time,
                    prep_time,
                    delta_time: submit_time,
                    root_time: flush_time,
                    io_time: Duration::ZERO,
                    total_time: block_start.elapsed(),
                    state_entries,
                    trie_entries: 0,
                });
            }

            *next_height += block_txs.len() as i64;
            total += iter_start.elapsed();
            if i == 0 {
                print_evm_stats(stats_label, &block_stats);
            }
        }
        total
    });
}

// ---------------------------------------------------------------------------
// QMDB EVM sync benchmark
// ---------------------------------------------------------------------------

fn bench_qmdb_evm_sync(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB evm sync");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut provider = InMemoryStateProvider::new();
    let addresses = setup_accounts(&mut provider, &mut rng);

    // ETH transfer workload
    let mut rng_blocks = StdRng::seed_from_u64(43);
    let eth_block_txs: Vec<GenericBlockTxs> =
        generate_block_txs(&addresses, &provider, NUM_BLOCKS, &mut rng_blocks)
            .into_iter()
            .map(|b| GenericBlockTxs { txs: b.to_eth_tx_envs() })
            .collect();

    // ERC20 transfer workload
    setup_erc20(&mut provider, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_block_txs = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    eprintln!("QMDB sync setup: {} accounts, {} txs/block", PRE_POP_ACCOUNTS, TXS_PER_BLOCK);

    // Each workload gets its own QMDB instance to avoid cross-contamination.
    let mut eth_state: Option<(AdsWrap<SingleCsTask>, i64, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("qmdb", &label), |b| {
        let (ads, next_height, _dir) = eth_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let config = QmdbConfig {
                dir: dir.path().to_str().unwrap().to_string(),
                ..QmdbConfig::default()
            };
            AdsCore::init_dir(&config);
            let mut ads = AdsWrap::<SingleCsTask>::new(&config);
            let num_pre_pop_blocks = qmdb_pre_populate(&mut ads, &provider);
            let next_height = num_pre_pop_blocks + 1;
            (ads, next_height, dir)
        });

        run_qmdb_sync_benchmark(
            b,
            &provider,
            &eth_block_txs,
            ads,
            next_height,
            "QMDB EVM ETH (sync)",
        );
    });

    let mut erc20_state: Option<(AdsWrap<SingleCsTask>, i64, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("erc20_qmdb", &label), |b| {
        let (ads, next_height, _dir) = erc20_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let config = QmdbConfig {
                dir: dir.path().to_str().unwrap().to_string(),
                ..QmdbConfig::default()
            };
            AdsCore::init_dir(&config);
            let mut ads = AdsWrap::<SingleCsTask>::new(&config);
            let num_pre_pop_blocks = qmdb_pre_populate(&mut ads, &provider);
            let next_height = num_pre_pop_blocks + 1;
            (ads, next_height, dir)
        });

        run_qmdb_sync_benchmark(
            b,
            &provider,
            &erc20_block_txs,
            ads,
            next_height,
            "QMDB EVM ERC20 (sync)",
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// QMDB EVM pipeline benchmark helper
// ---------------------------------------------------------------------------

fn run_qmdb_pipeline_benchmark(
    b: &mut criterion::Bencher<'_>,
    provider: &InMemoryStateProvider,
    block_txs: &[GenericBlockTxs],
    ads: &mut AdsWrap<SingleCsTask>,
    next_height: &mut i64,
    stats_label: &str,
) {
    b.iter_custom(|iters| {
        let mut total = Duration::ZERO;
        for i in 0..iters {
            let mut evm_provider = InMemoryStateProvider {
                accounts: provider.accounts.clone(),
                storage: provider.storage.clone(),
                code_by_hash: provider.code_by_hash.clone(),
            };

            let mut total_evm = Duration::ZERO;
            let mut total_prep = Duration::ZERO;
            let mut total_submit = Duration::ZERO;

            let iter_start = Instant::now();

            // Execute + submit all blocks (no per-block flush)
            for (blk_idx, blk) in block_txs.iter().enumerate() {
                let height = *next_height + blk_idx as i64;

                // EVM
                let (bundle, evm_time) =
                    execute_block_evm(&evm_provider, blk.txs.clone().into_iter(), blk_idx as u64 + 1);
                total_evm += evm_time;

                // Prep
                let prep_start = Instant::now();
                let (task, _) = bundle_state_to_qmdb_task(&bundle, OP_WRITE);
                total_prep += prep_start.elapsed();

                // Submit
                let submit_start = Instant::now();
                let task_id: i64 = height << IN_BLOCK_IDX_BITS;
                let tasks_manager =
                    Arc::new(TasksManager::new(vec![RwLock::new(Some(task))], task_id));
                ads.start_block(height, tasks_manager);
                let shared = ads.get_shared();
                shared.insert_extra_data(height, String::new());
                shared.add_task(task_id);
                total_submit += submit_start.elapsed();

                evm_provider.apply_bundle(&bundle);
            }

            // Single flush at the end
            let flush_start = Instant::now();
            ads.flush();
            let flush_time = flush_start.elapsed();

            *next_height += block_txs.len() as i64;
            let total_elapsed = iter_start.elapsed();
            total += total_elapsed;

            if i == 0 {
                let num = block_txs.len() as u32;
                eprintln!(
                    "─── {} ───\n  \
                     {} blocks avg: {:.2?}  (evm {:.2?}  prep {:.2?}  submit {:.2?}  flush {:.2?})\n",
                    stats_label,
                    num,
                    total_elapsed / num,
                    total_evm / num,
                    total_prep / num,
                    total_submit / num,
                    flush_time / num,
                );
            }
        }
        total
    });
}

// ---------------------------------------------------------------------------
// QMDB EVM pipeline benchmark
// ---------------------------------------------------------------------------

fn bench_qmdb_evm_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("QMDB evm pipeline");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    let label = format!("pre{PRE_POP_ACCOUNTS}_{NUM_BLOCKS}blk_{TXS_PER_BLOCK}tx");

    let mut rng = StdRng::seed_from_u64(42);
    let mut provider = InMemoryStateProvider::new();
    let addresses = setup_accounts(&mut provider, &mut rng);

    // ETH transfer workload
    let mut rng_blocks = StdRng::seed_from_u64(43);
    let eth_block_txs: Vec<GenericBlockTxs> =
        generate_block_txs(&addresses, &provider, NUM_BLOCKS, &mut rng_blocks)
            .into_iter()
            .map(|b| GenericBlockTxs { txs: b.to_eth_tx_envs() })
            .collect();

    // ERC20 transfer workload
    setup_erc20(&mut provider, &addresses);
    let mut rng_erc20 = StdRng::seed_from_u64(44);
    let erc20_block_txs = generate_erc20_block_txs(&addresses, NUM_BLOCKS, &mut rng_erc20);

    eprintln!("QMDB pipeline setup: {} accounts, {} txs/block", PRE_POP_ACCOUNTS, TXS_PER_BLOCK);

    // Each workload gets its own QMDB instance to avoid cross-contamination.
    let mut eth_state: Option<(AdsWrap<SingleCsTask>, i64, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("qmdb_pipeline", &label), |b| {
        let (ads, next_height, _dir) = eth_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let config = QmdbConfig {
                dir: dir.path().to_str().unwrap().to_string(),
                ..QmdbConfig::default()
            };
            AdsCore::init_dir(&config);
            let mut ads = AdsWrap::<SingleCsTask>::new(&config);
            let num_pre_pop_blocks = qmdb_pre_populate(&mut ads, &provider);
            let next_height = num_pre_pop_blocks + 1;
            (ads, next_height, dir)
        });

        run_qmdb_pipeline_benchmark(
            b,
            &provider,
            &eth_block_txs,
            ads,
            next_height,
            "QMDB EVM ETH (pipeline)",
        );
    });

    let mut erc20_state: Option<(AdsWrap<SingleCsTask>, i64, tempfile::TempDir)> = None;

    group.bench_function(BenchmarkId::new("erc20_qmdb_pipeline", &label), |b| {
        let (ads, next_height, _dir) = erc20_state.get_or_insert_with(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let config = QmdbConfig {
                dir: dir.path().to_str().unwrap().to_string(),
                ..QmdbConfig::default()
            };
            AdsCore::init_dir(&config);
            let mut ads = AdsWrap::<SingleCsTask>::new(&config);
            let num_pre_pop_blocks = qmdb_pre_populate(&mut ads, &provider);
            let next_height = num_pre_pop_blocks + 1;
            (ads, next_height, dir)
        });

        run_qmdb_pipeline_benchmark(
            b,
            &provider,
            &erc20_block_txs,
            ads,
            next_height,
            "QMDB EVM ERC20 (pipeline)",
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion main
// ---------------------------------------------------------------------------

criterion_group!(benches, bench_salt_evm, bench_qmdb_evm_sync, bench_qmdb_evm_pipeline);
criterion_main!(benches);
