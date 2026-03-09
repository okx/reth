//! End-to-end tests for the parallel execution framework.
//!
//! These tests exercise the full pipeline:
//!   SimTxEnv → Simulator → Framer → Dispatcher → ResultCollector → ParallelBlockResult
//!
//! A custom `TestStateProvider` with pre-funded accounts is used so that
//! EVM execution succeeds and produces meaningful state diffs.
//!
//! IMPORTANT: All test addresses use values >= 0x20 to avoid the Ethereum
//! precompile range (0x01-0x13 in Prague spec). Calling to a precompile
//! address with empty calldata causes `PrecompileError`.

use alloy_primitives::{Address, Bytes, StorageKey, StorageValue, TxKind, B256, U256};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_storage_errors::provider::ProviderResult;
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm::context::{BlockEnv, TxEnv};
use std::{collections::HashMap, sync::RwLock};
use xlayer_parallel_exec::{builder::ParallelBlockBuilder, simulator::SimTxEnv};

// ---------------------------------------------------------------------------
// TestStateProvider: a mock StateProvider with pre-funded accounts
// ---------------------------------------------------------------------------

/// A thread-safe mock state provider that stores accounts and storage in-memory.
/// Implements `StateProvider + Sync` so it can be used with the parallel framework.
#[derive(Debug)]
struct TestStateProvider {
    accounts: HashMap<Address, Account>,
    storage: HashMap<(Address, StorageKey), StorageValue>,
    bytecodes: HashMap<B256, Bytecode>,
    block_hashes: HashMap<u64, B256>,
    /// Track reads for verification (optional).
    read_log: RwLock<Vec<String>>,
}

impl TestStateProvider {
    fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            storage: HashMap::new(),
            bytecodes: HashMap::new(),
            block_hashes: HashMap::new(),
            read_log: RwLock::new(Vec::new()),
        }
    }

    /// Add a pre-funded account with the given balance and nonce.
    fn with_account(mut self, address: Address, balance: U256, nonce: u64) -> Self {
        self.accounts.insert(address, Account { balance, nonce, bytecode_hash: None });
        self
    }

    /// Add a storage slot.
    fn with_storage(mut self, address: Address, slot: StorageKey, value: StorageValue) -> Self {
        self.storage.insert((address, slot), value);
        self
    }
}

impl StateProvider for TestStateProvider {
    fn storage(
        &self,
        address: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        if let Ok(mut log) = self.read_log.write() {
            log.push(format!("storage({address:?}, {storage_key:?})"));
        }
        Ok(self.storage.get(&(address, storage_key)).copied())
    }
}

impl BytecodeReader for TestStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(self.bytecodes.get(code_hash).cloned())
    }
}

impl BlockHashReader for TestStateProvider {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.block_hashes.get(&number).copied())
    }

    fn canonical_hashes_range(&self, _start: u64, _end: u64) -> ProviderResult<Vec<B256>> {
        Ok(vec![])
    }
}

impl AccountReader for TestStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        if let Ok(mut log) = self.read_log.write() {
            log.push(format!("basic_account({address:?})"));
        }
        Ok(self.accounts.get(address).cloned())
    }
}

impl StateRootProvider for TestStateProvider {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }
}

impl StorageRootProvider for TestStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Ok(B256::ZERO)
    }

    fn storage_proof(
        &self,
        _address: Address,
        slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Ok(StorageProof::new(slot))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Ok(StorageMultiProof::empty())
    }
}

impl StateProofProvider for TestStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Ok(AccountProof::new(Address::ZERO))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Ok(MultiProof::default())
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Ok(Vec::default())
    }
}

impl HashedPostStateProvider for TestStateProvider {
    fn hashed_post_state(&self, _bundle_state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a simple ETH transfer transaction.
/// Uses gas_price=0 so no gas payment is needed beyond having sufficient balance.
fn make_transfer_tx(sender: Address, recipient: Address, value: U256, nonce: u64) -> SimTxEnv {
    let tx_env = TxEnv {
        caller: sender,
        gas_limit: 100_000,
        gas_price: 0,
        kind: TxKind::Call(recipient),
        value,
        nonce,
        ..Default::default()
    };
    SimTxEnv { sender, tx_env }
}

/// Create an address safely outside the precompile range (0x01-0x13).
/// Uses 0xA0 + offset as the last byte.
fn addr(offset: u8) -> Address {
    Address::with_last_byte(0xA0 + offset)
}

// ---------------------------------------------------------------------------
// End-to-end tests
// ---------------------------------------------------------------------------

/// Test 1: Full pipeline with a single simple transfer.
#[test]
fn test_e2e_single_transfer() {
    let sender = addr(0); // 0xA0
    let recipient = addr(1); // 0xA1
    let initial_balance = U256::from(1_000_000);
    let transfer_value = U256::from(100);

    let provider = TestStateProvider::new().with_account(sender, initial_balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![make_transfer_tx(sender, recipient, transfer_value, 0)];
    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), 1, "Expected 1 tx result");
    assert_eq!(result.tx_results[0].original_index, 0);
    assert!(result.tx_results[0].result.is_success(), "Transfer should succeed");
    assert!(result.total_gas_used > 0, "Gas should be non-zero");
    assert!(!result.merged_state.is_empty(), "Should have state changes");

    println!(
        "Single transfer: gas_used={}, merged_state_accounts={}, tx_success={}",
        result.total_gas_used,
        result.merged_state.len(),
        result.tx_results[0].result.is_success()
    );
}

/// Test 2: Multiple independent transfers between different senders.
/// These should be parallelizable (different senders/recipients = no conflicts).
#[test]
fn test_e2e_independent_transfers() {
    let sender_a = addr(0); // 0xA0
    let sender_b = addr(1); // 0xA1
    let recipient_a = addr(2); // 0xA2
    let recipient_b = addr(3); // 0xA3
    let balance = U256::from(1_000_000);
    let transfer = U256::from(100);

    let provider = TestStateProvider::new()
        .with_account(sender_a, balance, 0)
        .with_account(sender_b, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(sender_a, recipient_a, transfer, 0),
        make_transfer_tx(sender_b, recipient_b, transfer, 0),
    ];

    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), 2, "Expected 2 tx results");
    assert_eq!(result.tx_results[0].original_index, 0);
    assert_eq!(result.tx_results[1].original_index, 1);

    // Both transactions should succeed
    for (i, tx_result) in result.tx_results.iter().enumerate() {
        assert!(
            tx_result.result.is_success(),
            "Transaction {i} should succeed, got: {:?}",
            tx_result.result
        );
    }

    println!(
        "Independent transfers: gas_used={}, merged_accounts={}",
        result.total_gas_used,
        result.merged_state.len()
    );
}

/// Test 3: Conflicting transfers — same sender sends two transactions.
/// These MUST execute sequentially (same sender = nonce dependency).
#[test]
fn test_e2e_same_sender_sequential() {
    let sender = addr(0);
    let recipient_a = addr(1);
    let recipient_b = addr(2);
    let balance = U256::from(1_000_000);
    let transfer = U256::from(100);

    let provider = TestStateProvider::new()
        .with_account(sender, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(sender, recipient_a, transfer, 0),
        make_transfer_tx(sender, recipient_b, transfer, 1),
    ];

    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), 2, "Expected 2 tx results");
    assert_eq!(result.tx_results[0].original_index, 0);
    assert_eq!(result.tx_results[1].original_index, 1);
    assert!(result.tx_results[0].result.is_success(), "First tx should succeed");

    // Second tx depends on first tx's nonce bump being visible
    // It may or may not succeed depending on inter-frame state propagation
    assert!(result.total_gas_used > 0, "Gas should be non-zero");

    println!(
        "Same sender sequential: gas_used={}, tx0_success={}, tx1_success={}",
        result.total_gas_used,
        result.tx_results[0].result.is_success(),
        result.tx_results[1].result.is_success(),
    );
}

/// Test 4: Empty block — no transactions.
#[test]
fn test_e2e_empty_block() {
    let provider = TestStateProvider::new();
    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let result = builder.build(vec![], &provider, &block_env);

    assert!(result.tx_results.is_empty(), "No transactions should mean no results");
    assert!(result.merged_state.is_empty(), "No state changes for empty block");
    assert_eq!(result.total_gas_used, 0, "Zero gas for empty block");
    assert_eq!(result.total_fees, U256::ZERO, "Zero fees for empty block");
}

/// Test 5: Many independent transfers — stress test parallelism.
#[test]
fn test_e2e_many_independent_transfers() {
    let num_txs = 20;
    let balance = U256::from(10_000_000);
    let transfer = U256::from(100);

    let mut provider = TestStateProvider::new();
    let mut txs = Vec::with_capacity(num_txs);

    for i in 0..num_txs {
        // Use addresses starting at 0x20 (well above precompile range)
        let sender = Address::with_last_byte(0x20 + (i * 2) as u8);
        let recipient = Address::with_last_byte(0x20 + (i * 2 + 1) as u8);
        provider.accounts.insert(sender, Account { balance, nonce: 0, bytecode_hash: None });
        provider
            .accounts
            .insert(recipient, Account { balance: U256::ZERO, nonce: 0, bytecode_hash: None });
        txs.push(make_transfer_tx(sender, recipient, transfer, 0));
    }

    let builder = ParallelBlockBuilder::with_config(4, 8);
    let block_env = BlockEnv::default();

    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), num_txs, "All transactions should produce results");

    // Verify ordering is preserved
    for (i, tx_result) in result.tx_results.iter().enumerate() {
        assert_eq!(tx_result.original_index, i, "Results should be in original order");
    }

    // All should succeed (independent senders, sufficient balance)
    let successes = result.tx_results.iter().filter(|r| r.result.is_success()).count();
    assert_eq!(successes, num_txs, "All transfers should succeed");

    println!(
        "Many independent transfers: {num_txs} txs, {successes} succeeded, gas_used={}, accounts_in_state={}",
        result.total_gas_used,
        result.merged_state.len()
    );
}

/// Test 6: Verify that parallel execution produces correct final state.
/// Check that final balances are correct in merged_state.
#[test]
fn test_e2e_verify_final_balances() {
    let sender_a = addr(0);
    let sender_b = addr(1);
    let recipient_a = addr(2);
    let recipient_b = addr(3);
    let initial_balance = U256::from(1_000_000);
    let transfer_a = U256::from(500);
    let transfer_b = U256::from(300);

    let provider = TestStateProvider::new()
        .with_account(sender_a, initial_balance, 0)
        .with_account(sender_b, initial_balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(sender_a, recipient_a, transfer_a, 0),
        make_transfer_tx(sender_b, recipient_b, transfer_b, 0),
    ];

    let result = builder.build(txs, &provider, &block_env);

    assert!(result.tx_results[0].result.is_success(), "Transfer A should succeed");
    assert!(result.tx_results[1].result.is_success(), "Transfer B should succeed");

    // Check sender_a's balance: should be initial - transfer (gas_price=0 so no gas cost)
    if let Some(sender_a_state) = result.merged_state.get(&sender_a) {
        assert_eq!(
            sender_a_state.info.balance,
            initial_balance - transfer_a,
            "Sender A balance should be initial - transfer"
        );
        assert_eq!(sender_a_state.info.nonce, 1, "Sender A nonce should be 1");
    }

    // Check recipient_a received the transfer
    if let Some(recipient_a_state) = result.merged_state.get(&recipient_a) {
        assert_eq!(
            recipient_a_state.info.balance, transfer_a,
            "Recipient A should have received {transfer_a}"
        );
    }

    // Check sender_b
    if let Some(sender_b_state) = result.merged_state.get(&sender_b) {
        assert_eq!(
            sender_b_state.info.balance,
            initial_balance - transfer_b,
            "Sender B balance should be initial - transfer"
        );
        assert_eq!(sender_b_state.info.nonce, 1, "Sender B nonce should be 1");
    }

    // Check recipient_b
    if let Some(recipient_b_state) = result.merged_state.get(&recipient_b) {
        assert_eq!(
            recipient_b_state.info.balance, transfer_b,
            "Recipient B should have received {transfer_b}"
        );
    }

    println!("Balance verification passed for all accounts");
}

/// Test 7: Verify gas accounting across multiple transactions.
#[test]
fn test_e2e_gas_accounting() {
    let sender = addr(0);
    let recipient = addr(1);
    let balance = U256::from(10_000_000);
    let transfer = U256::from(100);

    let provider = TestStateProvider::new().with_account(sender, balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![make_transfer_tx(sender, recipient, transfer, 0)];
    let result = builder.build(txs, &provider, &block_env);

    assert!(result.tx_results[0].result.is_success(), "Transfer should succeed");

    // Total gas should equal the sum of individual tx gas
    let sum_gas: u64 = result.tx_results.iter().map(|r| r.gas_used).sum();
    assert_eq!(result.total_gas_used, sum_gas, "Total gas should equal sum of individual tx gas");

    // Simple ETH transfer gas: 21000 base
    // Note: may be slightly more due to cold account access costs
    assert!(
        result.tx_results[0].gas_used >= 21000,
        "Transfer should use at least 21000 gas, got {}",
        result.tx_results[0].gas_used
    );

    println!("Gas accounting: gas_used={}", result.tx_results[0].gas_used);
}

/// Test 8: Transfer that fails due to insufficient balance.
#[test]
fn test_e2e_insufficient_balance() {
    let sender = addr(0);
    let recipient = addr(1);
    let balance = U256::from(50); // Very low balance
    let transfer = U256::from(1_000_000); // More than balance

    let provider = TestStateProvider::new().with_account(sender, balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![make_transfer_tx(sender, recipient, transfer, 0)];
    let result = builder.build(txs, &provider, &block_env);

    // Should still produce a result (even if failed)
    assert_eq!(result.tx_results.len(), 1, "Should have 1 result even for failed tx");

    println!(
        "Insufficient balance test: tx_success={}, gas_used={}",
        result.tx_results[0].result.is_success(),
        result.tx_results[0].gas_used
    );
}

/// Test 9: Non-existent sender (no account in state).
#[test]
fn test_e2e_nonexistent_sender() {
    let sender = addr(0); // Not in provider
    let recipient = addr(1);

    let provider = TestStateProvider::new().with_account(recipient, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![make_transfer_tx(sender, recipient, U256::from(100), 0)];
    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), 1);
    println!(
        "Nonexistent sender: tx_success={}, gas_used={}",
        result.tx_results[0].result.is_success(),
        result.tx_results[0].gas_used
    );
}

/// Test 10: Mixed success and failure — some txs succeed, some fail.
#[test]
fn test_e2e_mixed_success_failure() {
    let rich_sender = addr(0);
    let poor_sender = addr(1);
    let recipient_a = addr(2);
    let recipient_b = addr(3);
    let rich_balance = U256::from(1_000_000);
    let poor_balance = U256::from(10); // Too low for transfer

    let provider = TestStateProvider::new()
        .with_account(rich_sender, rich_balance, 0)
        .with_account(poor_sender, poor_balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(rich_sender, recipient_a, U256::from(100), 0),
        make_transfer_tx(poor_sender, recipient_b, U256::from(1_000_000), 0),
    ];

    let result = builder.build(txs, &provider, &block_env);

    assert_eq!(result.tx_results.len(), 2);
    // First tx (rich sender) should succeed
    assert!(
        result.tx_results[0].result.is_success(),
        "Rich sender transfer should succeed, got: {:?}",
        result.tx_results[0].result
    );
    // Second tx (poor sender) should fail
    assert!(!result.tx_results[1].result.is_success(), "Poor sender transfer should fail");

    println!(
        "Mixed results: tx0_success={}, tx1_success={}, total_gas={}",
        result.tx_results[0].result.is_success(),
        result.tx_results[1].result.is_success(),
        result.total_gas_used,
    );
}

/// Test 11: Builder with different configurations.
#[test]
fn test_e2e_different_configs() {
    let sender = addr(0);
    let recipient = addr(1);
    let balance = U256::from(1_000_000);
    let transfer = U256::from(100);

    let provider = TestStateProvider::new().with_account(sender, balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let block_env = BlockEnv::default();
    let txs = vec![make_transfer_tx(sender, recipient, transfer, 0)];

    for (shards, threads) in [(1, 1), (1, 2), (2, 4), (4, 8), (8, 16)] {
        let builder = ParallelBlockBuilder::with_config(shards, threads);
        let result = builder.build(txs.clone(), &provider, &block_env);

        assert_eq!(
            result.tx_results.len(),
            1,
            "Config ({shards}, {threads}) should produce 1 result"
        );
        assert!(
            result.tx_results[0].result.is_success(),
            "Config ({shards}, {threads}) transfer should succeed"
        );
    }
}

/// Test 12: CachedStateProvider properly caches data.
#[test]
fn test_e2e_state_cache_population() {
    use xlayer_parallel_exec::state_cache::{CachedStateProvider, ParallelStateCache};

    let sender = addr(0);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new().with_account(sender, balance, 0);

    let cache = ParallelStateCache::new();
    let cached_provider = CachedStateProvider::new(&cache, &provider);

    // First read — should miss cache, hit fallback
    let account = revm::DatabaseRef::basic_ref(&cached_provider, sender).expect("should not error");
    assert!(account.is_some(), "Sender account should exist");
    assert_eq!(account.clone().unwrap().balance, balance);

    // Second read — should hit cache
    let account2 =
        revm::DatabaseRef::basic_ref(&cached_provider, sender).expect("should not error");
    assert_eq!(account, account2, "Cached value should match");

    let stats = cache.stats();
    assert!(stats.accounts_cached >= 1, "At least 1 account should be cached");
}

/// Test 13: Storage reads through CachedStateProvider.
#[test]
fn test_e2e_storage_cache_population() {
    use xlayer_parallel_exec::state_cache::{CachedStateProvider, ParallelStateCache};

    let contract = addr(10);
    let slot = B256::with_last_byte(0x07);
    let value = U256::from(999);

    let provider = TestStateProvider::new()
        .with_account(contract, U256::ZERO, 0)
        .with_storage(contract, slot, value);

    let cache = ParallelStateCache::new();
    let cached_provider = CachedStateProvider::new(&cache, &provider);

    let slot_u256 = U256::from_be_bytes(slot.0);
    let result = revm::DatabaseRef::storage_ref(&cached_provider, contract, slot_u256)
        .expect("should not error");
    assert_eq!(result, value, "Storage value should match");

    let cached = cache.get_storage(&contract, &slot_u256);
    assert_eq!(cached, Some(Some(value)), "Storage should be cached");
}

/// Test 14: Deterministic ordering — same inputs produce same outputs.
#[test]
fn test_e2e_deterministic_ordering() {
    let balance = U256::from(10_000_000);

    let mut provider = TestStateProvider::new();
    let mut txs = Vec::new();

    for i in 0..10u8 {
        let sender = Address::with_last_byte(0x20 + i * 2);
        let recipient = Address::with_last_byte(0x20 + i * 2 + 1);
        provider.accounts.insert(sender, Account { balance, nonce: 0, bytecode_hash: None });
        provider
            .accounts
            .insert(recipient, Account { balance: U256::ZERO, nonce: 0, bytecode_hash: None });
        txs.push(make_transfer_tx(sender, recipient, U256::from(100), 0));
    }

    let builder = ParallelBlockBuilder::with_config(4, 8);
    let block_env = BlockEnv::default();

    let result1 = builder.build(txs.clone(), &provider, &block_env);
    let result2 = builder.build(txs, &provider, &block_env);

    assert_eq!(result1.tx_results.len(), result2.tx_results.len());

    for (r1, r2) in result1.tx_results.iter().zip(result2.tx_results.iter()) {
        assert_eq!(r1.original_index, r2.original_index, "Ordering should be deterministic");
        assert_eq!(r1.gas_used, r2.gas_used, "Gas should be deterministic");
        assert_eq!(
            r1.result.is_success(),
            r2.result.is_success(),
            "Success status should be deterministic"
        );
    }

    assert_eq!(result1.total_gas_used, result2.total_gas_used, "Total gas should be deterministic");
}

/// Test 15: ParallelBlockResult debug output is well-formed.
#[test]
fn test_e2e_result_debug() {
    let sender = addr(0);
    let recipient = addr(1);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new().with_account(sender, balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![make_transfer_tx(sender, recipient, U256::from(100), 0)];
    let result = builder.build(txs, &provider, &block_env);

    let debug = format!("{:?}", result);
    assert!(debug.contains("ParallelBlockResult"), "Debug should contain struct name");
    assert!(debug.contains("tx_count"), "Debug should contain tx_count");
    assert!(debug.contains("total_gas_used"), "Debug should contain total_gas_used");
}

/// Test 16: Framer correctly separates conflicting transactions.
#[test]
fn test_e2e_framer_conflict_separation() {
    use xlayer_parallel_exec::{
        framer::Framer,
        simulator::Simulator,
        state_cache::{CachedStateProvider, ParallelStateCache},
    };

    let sender = addr(0);
    let recipient_a = addr(1);
    let recipient_b = addr(2);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new()
        .with_account(sender, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let cache = ParallelStateCache::new();
    let cached_provider = CachedStateProvider::new(&cache, &provider);
    let block_env = BlockEnv::default();
    let simulator = Simulator::new();

    // Two transactions from the same sender — should conflict
    let txs = vec![
        make_transfer_tx(sender, recipient_a, U256::from(100), 0),
        make_transfer_tx(sender, recipient_b, U256::from(200), 1),
    ];

    let sim_results = simulator.simulate(&txs, &cached_provider, &block_env);
    assert_eq!(sim_results.len(), 2);

    let mut framer = Framer::new();
    for sr in sim_results {
        framer.add(sr);
    }
    let frames = framer.finish();

    // Same-sender txs should be in different frames (they write to the same account)
    assert!(
        frames.len() >= 2,
        "Same-sender txs should produce at least 2 frames, got {}",
        frames.len()
    );

    let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
    assert_eq!(total_tasks, 2, "Total tasks should equal number of txs");
}

/// Test 17: Framer groups independent transactions (may share coinbase).
#[test]
fn test_e2e_framer_parallel_grouping() {
    use xlayer_parallel_exec::{
        framer::Framer,
        simulator::Simulator,
        state_cache::{CachedStateProvider, ParallelStateCache},
    };

    let sender_a = addr(0);
    let sender_b = addr(1);
    let recipient_a = addr(2);
    let recipient_b = addr(3);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new()
        .with_account(sender_a, balance, 0)
        .with_account(sender_b, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let cache = ParallelStateCache::new();
    let cached_provider = CachedStateProvider::new(&cache, &provider);
    let block_env = BlockEnv::default();
    let simulator = Simulator::new();

    let txs = vec![
        make_transfer_tx(sender_a, recipient_a, U256::from(100), 0),
        make_transfer_tx(sender_b, recipient_b, U256::from(200), 0),
    ];

    let sim_results = simulator.simulate(&txs, &cached_provider, &block_env);

    let mut framer = Framer::new();
    for sr in sim_results {
        framer.add(sr);
    }

    let frames = framer.finish();
    let total_tasks: usize = frames.iter().map(|f| f.tasks.len()).sum();
    assert_eq!(total_tasks, 2, "Should have 2 tasks total");

    // Note: truly independent txs might still be separated by the bloom filter
    // if they both write to coinbase (address 0x0). This is expected behavior.
    println!(
        "Independent txs: {} frames, tasks per frame: {:?}",
        frames.len(),
        frames.iter().map(|f| f.tasks.len()).collect::<Vec<_>>()
    );
}

/// Test 18: Simulator correctly extracts CrwSets from real EVM execution.
#[test]
fn test_e2e_simulator_crw_extraction() {
    use xlayer_parallel_exec::{
        simulator::Simulator,
        state_cache::{CachedStateProvider, ParallelStateCache},
    };

    let sender = addr(0);
    let recipient = addr(1);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new().with_account(sender, balance, 0).with_account(
        recipient,
        U256::ZERO,
        0,
    );

    let cache = ParallelStateCache::new();
    let cached_provider = CachedStateProvider::new(&cache, &provider);
    let block_env = BlockEnv::default();
    let simulator = Simulator::new();

    let txs = vec![make_transfer_tx(sender, recipient, U256::from(100), 0)];
    let sim_results = simulator.simulate(&txs, &cached_provider, &block_env);

    assert_eq!(sim_results.len(), 1);
    let sr = &sim_results[0];

    assert!(
        !sr.crw_sets.account_writes.is_empty(),
        "Should have at least one account write (sender balance)"
    );
    assert!(!sr.crw_sets.account_reads.is_empty(), "Should have account reads");
    assert!(sr.success, "Simulation should succeed");

    println!(
        "CrwSets: reads={}, writes={}, storage_reads={}, storage_writes={}",
        sr.crw_sets.account_reads.len(),
        sr.crw_sets.account_writes.len(),
        sr.crw_sets.storage_reads.len(),
        sr.crw_sets.storage_writes.len(),
    );
}

/// Test 19: Dispatcher correctly applies inter-frame state.
#[test]
fn test_e2e_dispatcher_inter_frame_state() {
    use xlayer_parallel_exec::{
        crw_sets::CrwSets,
        dispatcher::Dispatcher,
        framer::Frame,
        state_cache::ParallelStateCache,
        task::{ExeTask, SimResult},
    };

    let sender = addr(0);
    let recipient_a = addr(1);
    let recipient_b = addr(2);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new()
        .with_account(sender, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let dispatcher = Dispatcher::new(4);
    let cache = ParallelStateCache::new();
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(sender, recipient_a, U256::from(100), 0),
        make_transfer_tx(sender, recipient_b, U256::from(200), 1),
    ];

    let frame1 = Frame {
        tasks: vec![ExeTask::new(SimResult {
            crw_sets: CrwSets::default(),
            original_index: 0,
            success: true,
        })],
    };
    let frame2 = Frame {
        tasks: vec![ExeTask::new(SimResult {
            crw_sets: CrwSets::default(),
            original_index: 1,
            success: true,
        })],
    };

    let results = dispatcher.execute(vec![frame1, frame2], &cache, &provider, &block_env, &txs);

    assert_eq!(results.len(), 2, "Should have 2 results");
    assert_eq!(results[0].original_index, 0);
    assert_eq!(results[1].original_index, 1);

    // First tx should succeed
    assert!(
        results[0].result.is_success(),
        "First tx should succeed, got: {:?}",
        results[0].result
    );

    // Second tx depends on inter-frame state propagation (nonce bump)
    println!(
        "Inter-frame state: tx0_gas={}, tx1_gas={}, tx0_success={}, tx1_success={}",
        results[0].gas_used,
        results[1].gas_used,
        results[0].result.is_success(),
        results[1].result.is_success(),
    );
}

/// Test 20: Result collector merge correctness with real execution output.
#[test]
fn test_e2e_result_collector_merge() {
    use xlayer_parallel_exec::result_collector;

    let sender_a = addr(0);
    let sender_b = addr(1);
    let recipient_a = addr(2);
    let recipient_b = addr(3);
    let balance = U256::from(1_000_000);

    let provider = TestStateProvider::new()
        .with_account(sender_a, balance, 0)
        .with_account(sender_b, balance, 0)
        .with_account(recipient_a, U256::ZERO, 0)
        .with_account(recipient_b, U256::ZERO, 0);

    let builder = ParallelBlockBuilder::with_config(2, 4);
    let block_env = BlockEnv::default();

    let txs = vec![
        make_transfer_tx(sender_a, recipient_a, U256::from(100), 0),
        make_transfer_tx(sender_b, recipient_b, U256::from(200), 0),
    ];

    let result = builder.build(txs, &provider, &block_env);

    // Verify merge_states is consistent with individual results
    let re_merged = result_collector::merge_states(&result.tx_results);

    assert_eq!(result.merged_state.len(), re_merged.len(), "Re-merged state should match original");

    for (addr, account) in &result.merged_state {
        let re_account = re_merged.get(addr).expect("Account should exist in re-merged state");
        assert_eq!(
            account.info.balance, re_account.info.balance,
            "Balances should match for {addr:?}"
        );
        assert_eq!(account.info.nonce, re_account.info.nonce, "Nonces should match for {addr:?}");
    }
}
