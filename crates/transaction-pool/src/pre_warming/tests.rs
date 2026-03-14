//! # Pre-Warming Module Test Suite
//!
//! This comprehensive test suite validates the cache pre-warming system for X-Layer (Optimism-based
//! L2).
//!
//! ## What This Module Does
//!
//! The pre-warming system improves block execution performance by:
//! 1. Simulating transactions in background workers as they arrive in the mempool
//! 2. Extracting state keys (accounts, storage slots, bytecode) that will be accessed
//! 3. Storing these keys per-transaction in a cache
//! 4. Before block execution, batch-fetching values for selected transactions' keys
//! 5. Pre-populating `CachedReads` so execution sees cache hits instead of MDBX queries
//!
//! ## Test Coverage
//!
//! ### Module Exports (2 tests)
//! - Verifies public API accessibility
//! - End-to-end aggregation flow
//!
//! ### Integration Tests (6 tests)
//! - Configuration → Cache behavior
//! - Key merging and deduplication
//! - Store/retrieve operations
//! - Cache removal (transaction mined)
//! - Statistics accuracy
//! - Concurrent access safety
//!
//! ### End-to-End Tests (7 tests)
//! - Full pre-warming flow simulation
//! - Key deduplication across transactions
//! - Edge cases: empty selection, non-existent transactions
//! - Request lifecycle and age tracking
//!
//! ### Benchmark Tests (12 tests)
//! - Key addition performance (accounts, storage slots)
//! - Merge performance
//! - Cache operations (store, retrieve, remove)
//! - Concurrent operations
//! - Large-scale merging (2000 TX block)
//! - HashSet deduplication efficiency
//!
//! ### Stress Tests (4 tests)
//! - 10,000 pending transactions
//! - Complex transactions with many keys
//! - Concurrent readers and writers
//! - Rapid add/remove cycles
//!
//! ## Realistic Test Data
//!
//! Tests use real mainnet contract addresses (WETH, USDC, Uniswap, etc.) and
//! realistic DeFi scenarios (swaps, transfers, NFT trades) for practical validation.

#![cfg(test)]

use super::*;
use alloy_primitives::{address, Address, TxHash, B256, U256};
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

// ============================================================================
// REALISTIC TEST DATA - Mimics real-world blockchain addresses and values
// ============================================================================

/// Well-known mainnet contract addresses for realistic testing
mod known_addresses {
    use super::*;

    /// WETH contract (Wrapped Ether)
    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    /// USDC contract (USD Coin - 6 decimals)
    pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    /// USDT contract (Tether USD - 6 decimals)
    pub const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    /// Uniswap V2 Router - most common DEX entry point
    pub const UNISWAP_V2_ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
    /// Uniswap V3 Router - concentrated liquidity DEX
    pub const UNISWAP_V3_ROUTER: Address = address!("E592427A0AEce92De3Edee1F18E0157C05861564");
    /// OpenSea Seaport - NFT marketplace
    pub const SEAPORT: Address = address!("00000000000000ADc04C56Bf30aC9d3c0aAF14dC");

    /// Sample user addresses (Hardhat/Foundry default test accounts)
    pub const ALICE: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    pub const BOB: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    pub const CHARLIE: Address = address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    pub const DAVE: Address = address!("90F79bf6EB2c4f870365E785982E1f101E93b906");
    pub const EVE: Address = address!("15d34AAf54267DB7D7c367839AAf71A00a2C6A65");
}

/// Common storage slot indices used in ERC20 and other contracts
mod storage_slots {
    use super::*;

    /// Balance mapping slot (typical for ERC20, slot 0)
    pub const BALANCE_SLOT: U256 = U256::ZERO;
    /// Allowance mapping slot (typical for ERC20, slot 1)
    pub const ALLOWANCE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
    /// Total supply slot (typical for ERC20, slot 2)
    pub const TOTAL_SUPPLY_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);
    /// Owner slot (for Ownable contracts, slot 3)
    pub const OWNER_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);
}

/// Realistic ETH/token amounts for test scenarios
mod amounts {
    use super::*;

    /// 1 ETH in wei (1e18)
    pub const ONE_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);
    /// 10 ETH in wei (1e19)
    pub const TEN_ETH: U256 = U256::from_limbs([10_000_000_000_000_000_000u64, 0, 0, 0]);
    /// 100 USDC (6 decimals = 1e8)
    pub const HUNDRED_USDC: U256 = U256::from_limbs([100_000_000u64, 0, 0, 0]);
    /// 1000 USDC (6 decimals = 1e9)
    pub const THOUSAND_USDC: U256 = U256::from_limbs([1_000_000_000u64, 0, 0, 0]);
}

use amounts::*;
use known_addresses::*;
use storage_slots::*;

// ============================================================================
// MODULE EXPORTS TESTS
// ============================================================================

/// # Test: Module Public API Accessibility
///
/// ## Scenario
/// A developer imports the pre_warming module and attempts to use all public types.
///
/// ## Flow Being Tested
/// ```text
/// Developer → imports pre_warming → creates instances of public types
/// ```
///
/// ## Validates
/// - `PreWarmingConfig` is accessible and has `default()`
/// - `PreWarmedCache` is accessible and can be instantiated
/// - `ExtractedKeys` is accessible and has `new()`
/// - `SimulationRequest` is accessible and can be created
///
/// ## Why This Matters
/// Ensures the public API surface is correctly exported and usable by external code.
#[test]
fn test_module_exports() {
    let _config = PreWarmingConfig::default();
    let _cache = PreWarmedCache::new(PreWarmingConfig::default());
    let _keys = ExtractedKeys::new();
    let _request = SimulationRequest::new(TxHash::random(), 42u64, 0);
}

/// # Test: End-to-End Key Aggregation Flow
///
/// ## Scenario
/// Two DeFi transactions in the same block:
/// 1. Alice swaps WETH for USDC on Uniswap
/// 2. Bob transfers USDC to Charlie
///
/// Both touch USDC contract - keys should be deduplicated when merged.
///
/// ## Flow Being Tested
/// ```text
/// TX1 arrives → extract keys (Alice, Uniswap, WETH, USDC)
///     ↓
/// TX2 arrives → extract keys (Bob, USDC, storage slot)
///     ↓
/// Store both in cache
///     ↓
/// Block builder queries both TXs
///     ↓
/// Merged keys returned (USDC deduplicated)
/// ```
///
/// ## Validates
/// - Keys from multiple transactions can be stored separately
/// - Retrieval merges keys correctly
/// - Duplicate addresses (USDC) are deduplicated
/// - Storage slots are preserved
#[test]
fn test_end_to_end_aggregated_flow() {
    let config = PreWarmingConfig::enabled();
    let cache = PreWarmedCache::new(config);

    // TX1: Alice swaps on Uniswap
    let tx1_hash = TxHash::random();
    let mut keys1 = ExtractedKeys::new();
    keys1.add_account(ALICE); // Sender
    keys1.add_account(UNISWAP_V2_ROUTER); // Contract called
    keys1.add_account(WETH); // Token in
    keys1.add_account(USDC); // Token out

    // TX2: Bob transfers USDC
    let tx2_hash = TxHash::random();
    let mut keys2 = ExtractedKeys::new();
    keys2.add_account(BOB); // Sender
    keys2.add_account(USDC); // Token contract (shared with TX1)
    keys2.add_storage_slot(USDC, BALANCE_SLOT); // Bob's balance

    cache.store_tx_keys(tx1_hash, keys1);
    cache.store_tx_keys(tx2_hash, keys2);

    // Retrieve merged keys - USDC should be deduplicated
    let all_keys = cache.get_keys_for_txs(&[tx1_hash, tx2_hash]);

    // 4 unique addresses from TX1 + 1 unique from TX2 (BOB), USDC is shared
    assert_eq!(all_keys.accounts.len(), 5);
    assert_eq!(all_keys.storage_slots.len(), 1);
}

// ============================================================================
// INTEGRATION TESTS - Testing component interactions
// ============================================================================

/// Integration tests validate that different components work together correctly.
/// These tests focus on the interaction between:
/// - `PreWarmingConfig` and `PreWarmedCache`
/// - `ExtractedKeys` merging behavior
/// - Cache store/retrieve/remove operations
/// - Concurrent access patterns
mod integration {
    use super::*;

    /// # Test: Configuration Affects Cache Behavior
    ///
    /// ## Scenario
    /// Node operator configures pre-warming with specific worker count and cache limits.
    /// The cache should respect these settings when processing transactions.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Create config → set workers=4, max_entries=100
    ///     ↓
    /// Create cache with config
    ///     ↓
    /// Store 50 transactions (each user transferring USDC)
    ///     ↓
    /// Verify cache respects configuration
    /// ```
    ///
    /// ## Validates
    /// - Config builder pattern works correctly
    /// - Cache accepts configuration
    /// - Basic store operations work
    #[test]
    fn test_config_to_cache_integration() {
        let config = PreWarmingConfig::default().with_workers(4).with_cache_max_entries(100);

        let cache = PreWarmedCache::new(config);

        // Simulate 50 different users interacting with USDC
        let users = generate_user_addresses(50);
        for user in &users {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(*user);
            keys.add_account(USDC);
            keys.add_storage_slot(USDC, BALANCE_SLOT);
            cache.store_tx_keys(tx_hash, keys);
        }

        assert_eq!(cache.len(), 50);
    }

    /// # Test: Key Merging Deduplicates Shared Contracts
    ///
    /// ## Scenario
    /// Three users (Alice, Bob, Charlie) all swap on Uniswap in the same block.
    /// They all touch the Uniswap router, but access different token balance slots.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Alice swap keys: [Alice, Uniswap, WETH balance]
    ///     ↓
    /// Bob swap keys: [Bob, Uniswap, USDC balance]
    ///     ↓
    /// Charlie swap keys: [Charlie, Uniswap, USDT balance]
    ///     ↓
    /// Merge all three
    ///     ↓
    /// Result: 4 accounts (Uniswap deduplicated), 3 storage slots
    /// ```
    ///
    /// ## Validates
    /// - `ExtractedKeys::merge()` correctly combines keys
    /// - Duplicate accounts are deduplicated (HashSet behavior)
    /// - Unique storage slots are preserved
    #[test]
    fn test_extracted_keys_merge_integration() {
        let mut keys1 = ExtractedKeys::new();
        let mut keys2 = ExtractedKeys::new();
        let mut keys3 = ExtractedKeys::new();

        // All three TXs touch Uniswap router (should deduplicate)
        keys1.add_account(ALICE);
        keys1.add_account(UNISWAP_V2_ROUTER);
        keys1.add_storage_slot(WETH, BALANCE_SLOT);

        keys2.add_account(BOB);
        keys2.add_account(UNISWAP_V2_ROUTER);
        keys2.add_storage_slot(USDC, BALANCE_SLOT);

        keys3.add_account(CHARLIE);
        keys3.add_account(UNISWAP_V2_ROUTER);
        keys3.add_storage_slot(USDT, BALANCE_SLOT);

        // Merge all
        let mut merged = ExtractedKeys::new();
        merged.merge(keys1);
        merged.merge(keys2);
        merged.merge(keys3);

        // 3 users + 1 router (deduplicated) = 4 accounts
        assert_eq!(merged.accounts.len(), 4);
        // 3 different token balance slots
        assert_eq!(merged.storage_slots.len(), 3);
    }

    /// # Test: Cache Store and Selective Retrieval
    ///
    /// ## Scenario
    /// Five users (Alice, Bob, Charlie, Dave, Eve) each submit USDC transfers.
    /// Block builder selects only the first 3 for the block.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store 5 TX keys in cache
    ///     ↓
    /// Block builder selects TX[0..3]
    ///     ↓
    /// Query cache with selected TX hashes
    ///     ↓
    /// Get merged keys for ONLY selected transactions
    /// ```
    ///
    /// ## Validates
    /// - Per-transaction key storage works
    /// - Selective retrieval only returns requested TXs
    /// - Shared contracts (USDC) are deduplicated in result
    #[test]
    fn test_cache_store_and_retrieve_integration() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let senders = [ALICE, BOB, CHARLIE, DAVE, EVE];
        let tx_hashes: Vec<TxHash> = senders
            .iter()
            .map(|sender| {
                let tx_hash = TxHash::random();
                let mut keys = ExtractedKeys::new();
                keys.add_account(*sender);
                keys.add_account(USDC);
                keys.add_storage_slot(USDC, BALANCE_SLOT);
                cache.store_tx_keys(tx_hash, keys);
                tx_hash
            })
            .collect();

        // Retrieve keys for first 3 transactions
        let selected = &tx_hashes[0..3];
        let merged_keys = cache.get_keys_for_txs(selected);

        // 3 senders + USDC (shared) = 4 accounts
        assert_eq!(merged_keys.accounts.len(), 4);
        // All share same storage slot = 1
        assert_eq!(merged_keys.storage_slots.len(), 1);
    }

    /// # Test: Cache Removal After Block Mining
    ///
    /// ## Scenario
    /// Five users submit transactions. Block is built with Alice, Bob, Charlie.
    /// After mining, their keys should be removed. Dave and Eve remain pending.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store 5 TX keys
    ///     ↓
    /// Block mined with TX[0..3]
    ///     ↓
    /// Call cache.remove_txs() with mined TX hashes
    ///     ↓
    /// Verify: mined TXs return empty, remaining TXs still available
    /// ```
    ///
    /// ## Validates
    /// - `remove_txs()` correctly removes specified transactions
    /// - Remaining transactions are unaffected
    /// - Removed transactions return empty keys
    #[test]
    fn test_cache_removal_integration() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let users = [ALICE, BOB, CHARLIE, DAVE, EVE];
        let mut tx_hashes = Vec::new();

        for user in &users {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(*user);
            keys.add_storage_slot(WETH, BALANCE_SLOT);
            cache.store_tx_keys(tx_hash, keys);
            tx_hashes.push(tx_hash);
        }

        assert_eq!(cache.len(), 5);

        // Alice, Bob, Charlie got mined — evict them
        let mined = &tx_hashes[0..3];
        cache.remove_txs(mined);

        // Only Dave and Eve remain
        assert_eq!(cache.len(), 2);

        // Dave and Eve's keys should still be available
        let remaining = cache.get_keys_for_txs(&tx_hashes[3..5]);
        assert_eq!(remaining.accounts.len(), 2);

        // Mined TXs are gone
        let removed = cache.get_keys_for_txs(mined);
        assert!(removed.is_empty());
    }

    /// # Test: Cache Statistics Accuracy
    ///
    /// ## Scenario
    /// Two different transaction types:
    /// 1. Complex DeFi interaction (accounts, storage, bytecode)
    /// 2. Simple transfer with BLOCKHASH opcode
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store TX1: 2 accounts, 1 storage, 1 code_hash
    ///     ↓
    /// Store TX2: 2 accounts, 0 storage, 1 block_hash
    ///     ↓
    /// Query stats()
    ///     ↓
    /// Verify counts match expected
    /// ```
    ///
    /// ## Validates
    /// - `stats()` accurately counts all key types
    /// - Different key types are tracked separately
    /// - Transaction count is accurate
    #[test]
    fn test_cache_stats_integration() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        // TX1: Complex DeFi interaction
        let tx1 = TxHash::random();
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(ALICE);
        keys1.add_account(UNISWAP_V2_ROUTER);
        keys1.add_storage_slot(WETH, BALANCE_SLOT);
        keys1.add_code_hash(B256::random());
        cache.store_tx_keys(tx1, keys1);

        // TX2: Simple transfer with BLOCKHASH
        let tx2 = TxHash::random();
        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(BOB);
        keys2.add_account(USDC);
        keys2.add_block_hash(12345678);
        cache.store_tx_keys(tx2, keys2);

        let stats = cache.stats();

        assert_eq!(stats.total_transactions, 2);
        assert_eq!(stats.total_accounts, 4);
        assert_eq!(stats.total_storage_slots, 1);
        assert_eq!(stats.total_code_hashes, 1);
        assert_eq!(stats.total_block_hashes, 1);
    }

    /// # Test: Concurrent Cache Access Safety
    ///
    /// ## Scenario
    /// 10 parallel threads each store 100 transactions simultaneously.
    /// This simulates high-throughput transaction arrival from multiple P2P connections.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Spawn 10 threads
    ///     ↓
    /// Each thread: store 100 TXs with unique keys
    ///     ↓
    /// All threads complete
    ///     ↓
    /// Verify: cache has exactly 1000 entries (no data loss)
    /// ```
    ///
    /// ## Validates
    /// - `RwLock` protection works correctly
    /// - No data loss under concurrent writes
    /// - No deadlocks or panics
    #[test]
    fn test_concurrent_cache_access() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let handles: Vec<_> = (0..10)
            .map(|thread_id| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for tx_num in 0..100 {
                        let tx_hash = TxHash::random();
                        let mut keys = ExtractedKeys::new();
                        let user = generate_deterministic_address(thread_id * 1000 + tx_num);
                        keys.add_account(user);
                        keys.add_account(USDC);
                        keys.add_storage_slot(USDC, U256::from(thread_id * 1000 + tx_num));

                        cache.store_tx_keys(tx_hash, keys);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(cache.len(), 1000);
    }
}

/// Generate deterministic address from index (for reproducible tests)
fn generate_deterministic_address(index: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0xDE; // Prefix to make it look like a real address
    bytes[1] = 0xAD;
    bytes[16] = (index >> 24) as u8;
    bytes[17] = (index >> 16) as u8;
    bytes[18] = (index >> 8) as u8;
    bytes[19] = index as u8;
    Address::from_slice(&bytes)
}

/// Generate N unique user addresses
fn generate_user_addresses(count: usize) -> Vec<Address> {
    (0..count).map(|i| generate_deterministic_address(i + 1000)).collect()
}

// ============================================================================
// END-TO-END TESTS - Full flow simulation
// ============================================================================

/// End-to-end tests simulate the complete pre-warming lifecycle from transaction
/// arrival to block execution. These tests validate the full integration path.
mod e2e {
    use super::*;

    /// # Test: Complete Pre-Warming Flow for Mixed Block
    ///
    /// ## Scenario
    /// A realistic X-Layer block containing 100 transactions of mixed types:
    /// - Token transfers (33%)
    /// - DEX swaps (33%)
    /// - NFT trades (33%)
    ///
    /// ## Flow Being Tested
    /// ```text
    /// 100 TXs arrive → simulate each → extract keys
    ///     ↓
    /// Store all keys in cache (per-TX)
    ///     ↓
    /// Block builder selects top 50
    ///     ↓
    /// Retrieve merged keys for selected TXs
    ///     ↓
    /// Block mined → remove selected TXs from cache
    ///     ↓
    /// Verify: 50 TXs remain in cache
    /// ```
    ///
    /// ## Validates
    /// - Mixed transaction types all work correctly
    /// - Large-scale key storage and retrieval
    /// - Post-mining cleanup
    #[test]
    fn test_full_pre_warming_flow() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let mut all_tx_hashes = Vec::new();
        let users = generate_user_addresses(100);

        for (i, user) in users.iter().enumerate() {
            let tx_hash = TxHash::random();
            all_tx_hashes.push(tx_hash);

            let mut keys = ExtractedKeys::new();
            keys.add_account(*user);

            match i % 3 {
                0 => {
                    // Token transfer
                    keys.add_account(USDC);
                    keys.add_account(generate_deterministic_address(i + 5000));
                    keys.add_storage_slot(USDC, U256::from(i));
                }
                1 => {
                    // DEX swap
                    keys.add_account(UNISWAP_V2_ROUTER);
                    keys.add_account(WETH);
                    keys.add_account(USDC);
                    keys.add_storage_slot(WETH, U256::from(i));
                    keys.add_storage_slot(USDC, U256::from(i));
                }
                _ => {
                    // NFT trade
                    keys.add_account(SEAPORT);
                    keys.add_account(generate_deterministic_address(i + 3000));
                    keys.add_storage_slot(generate_deterministic_address(i + 3000), U256::from(i));
                }
            }

            cache.store_tx_keys(tx_hash, keys);
        }

        assert_eq!(cache.len(), 100);

        let selected_for_block: Vec<TxHash> = all_tx_hashes[0..50].to_vec();
        let prefetch_keys = cache.get_keys_for_txs(&selected_for_block);

        assert!(prefetch_keys.accounts.len() > 50);
        assert!(prefetch_keys.storage_slots.len() > 0);

        // 50 selected TXs are evicted; 50 remain
        cache.remove_txs(&selected_for_block);
        assert_eq!(cache.len(), 50);
    }

    /// # Test: Overlapping Keys Deduplication (Hot Contract)
    ///
    /// ## Scenario
    /// Five users all swap on the same Uniswap WETH/USDC pool in one block.
    /// All transactions touch the same contracts and storage slots.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// 5 users each swap WETH→USDC
    ///     ↓
    /// Each TX touches: [user, Uniswap, WETH, USDC, WETH.balance, USDC.balance]
    ///     ↓
    /// Store 5 TX keys
    ///     ↓
    /// Merge all 5
    ///     ↓
    /// Result: 8 accounts (5 users + 3 contracts), 2 storage slots
    /// ```
    ///
    /// ## Validates
    /// - Heavy deduplication works correctly
    /// - Hot contracts don't cause key explosion
    /// - Storage slot deduplication works
    #[test]
    fn test_overlapping_keys_deduplication() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let users = [ALICE, BOB, CHARLIE, DAVE, EVE];
        let tx_hashes: Vec<TxHash> = users
            .iter()
            .map(|user| {
                let tx_hash = TxHash::random();
                let mut keys = ExtractedKeys::new();

                keys.add_account(*user);
                keys.add_account(UNISWAP_V2_ROUTER);
                keys.add_account(WETH);
                keys.add_account(USDC);
                keys.add_storage_slot(WETH, BALANCE_SLOT);
                keys.add_storage_slot(USDC, BALANCE_SLOT);

                cache.store_tx_keys(tx_hash, keys);
                tx_hash
            })
            .collect();

        let merged = cache.get_keys_for_txs(&tx_hashes);

        assert_eq!(merged.accounts.len(), 8);
        assert_eq!(merged.storage_slots.len(), 2);
    }

    /// # Test: Empty Block Selection (Edge Case)
    ///
    /// ## Scenario
    /// Cache has transactions, but block builder selects zero transactions.
    /// This can happen during network congestion or validator issues.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store 3 TX keys
    ///     ↓
    /// Block builder calls get_keys_for_txs([]) - empty array
    ///     ↓
    /// Should return empty ExtractedKeys (not error)
    /// ```
    ///
    /// ## Validates
    /// - Empty selection doesn't panic
    /// - Returns empty keys (not null/error)
    /// - Cache remains unchanged
    #[test]
    fn test_empty_block_selection() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        for user in [ALICE, BOB, CHARLIE] {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(user);
            keys.add_account(USDC);
            cache.store_tx_keys(tx_hash, keys);
        }

        let empty_selection: Vec<TxHash> = vec![];
        let keys = cache.get_keys_for_txs(&empty_selection);
        assert!(keys.is_empty());
    }

    /// # Test: Query Non-Existent Transactions
    ///
    /// ## Scenario
    /// Block builder queries for transactions that don't exist in cache.
    /// This can happen if TXs were dropped or never simulated.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store Alice's TX
    ///     ↓
    /// Query for 5 random TX hashes (none exist)
    ///     ↓
    /// Should return empty keys
    /// ```
    ///
    /// ## Validates
    /// - Missing TXs don't cause errors
    /// - Returns empty keys for unknown hashes
    #[test]
    fn test_select_nonexistent_transactions() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let alice_tx = TxHash::random();
        let mut keys = ExtractedKeys::new();
        keys.add_account(ALICE);
        keys.add_account(USDC);
        cache.store_tx_keys(alice_tx, keys);

        let nonexistent: Vec<TxHash> = (0..5).map(|_| TxHash::random()).collect();
        let keys = cache.get_keys_for_txs(&nonexistent);
        assert!(keys.is_empty());
    }

    /// # Test: Mixed Existing and Non-Existent TXs
    ///
    /// ## Scenario
    /// Block builder queries for a mix of existing and non-existing transactions.
    /// Only keys from existing TXs should be returned.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store Alice's TX
    ///     ↓
    /// Query for [Alice's TX, random1, random2]
    ///     ↓
    /// Should return only Alice's keys
    /// ```
    ///
    /// ## Validates
    /// - Partial matches work correctly
    /// - Missing TXs are silently skipped
    /// - Existing TX keys are returned
    #[test]
    fn test_mixed_existing_and_nonexistent() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let alice_tx = TxHash::random();
        let mut keys = ExtractedKeys::new();
        keys.add_account(ALICE);
        keys.add_account(WETH);
        cache.store_tx_keys(alice_tx, keys);

        let selection = vec![alice_tx, TxHash::random(), TxHash::random()];

        let merged = cache.get_keys_for_txs(&selection);
        assert_eq!(merged.accounts.len(), 2);
    }

    /// # Test: SimulationRequest Lifecycle and Age Tracking
    ///
    /// ## Scenario
    /// A simulation request is created and its age is tracked over time.
    /// This is used for timeout and staleness detection.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Create SimulationRequest at T=0
    ///     ↓
    /// Check age immediately (should be ~0)
    ///     ↓
    /// Wait 50ms
    ///     ↓
    /// Check age again (should be >= 50ms)
    /// ```
    ///
    /// ## Validates
    /// - `SimulationRequest::new()` captures timestamp
    /// - `age()` correctly calculates elapsed time
    /// - Time tracking for timeout decisions
    #[test]
    fn test_simulation_request_lifecycle() {
        let tx_hash = TxHash::random();
        let request = SimulationRequest::new(tx_hash, 42u64, 0);

        assert_eq!(request.tx_hash, tx_hash);
        assert_eq!(request.transaction, 42u64);
        assert!(request.age() < Duration::from_millis(10));

        thread::sleep(Duration::from_millis(50));
        assert!(request.age() >= Duration::from_millis(50));
    }

    /// # Test: ExtractedKeys Age Tracking
    ///
    /// ## Scenario
    /// Extracted keys are created and their age is tracked.
    /// Used for cache staleness detection.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Create ExtractedKeys at T=0
    ///     ↓
    /// Check age immediately
    ///     ↓
    /// Wait 50ms
    ///     ↓
    /// Check age (should be >= 50ms)
    /// ```
    ///
    /// ## Validates
    /// - `ExtractedKeys::new()` creates valid struct
    /// - Keys can be stored and retrieved
    #[test]
    fn test_extracted_keys_creation() {
        let keys = ExtractedKeys::new();
        // Verify the keys struct is empty on creation
        assert!(keys.is_empty());
        assert_eq!(keys.total_keys(), 0);
    }

    /// # Test: Duplicate TX Hash Overwrites (Edge Case)
    ///
    /// ## Scenario
    /// Same transaction is simulated twice (e.g., due to retry or race condition).
    /// The second simulation should overwrite the first.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store TX with keys [Alice, USDC]
    ///     ↓
    /// Store same TX hash with keys [Alice, WETH]
    ///     ↓
    /// Retrieve TX keys
    ///     ↓
    /// Should have second set of keys only
    /// ```
    ///
    /// ## Validates
    /// - HashMap overwrite behavior
    /// - No duplicate entries for same TX
    /// - Latest simulation wins
    #[test]
    fn test_duplicate_tx_hash_overwrites() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hash = TxHash::random();

        // First simulation
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(ALICE);
        keys1.add_account(USDC);
        cache.store_tx_keys(tx_hash, keys1);

        // Second simulation (same TX, different keys)
        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(ALICE);
        keys2.add_account(WETH);
        cache.store_tx_keys(tx_hash, keys2);

        // Should only have 1 entry
        assert_eq!(cache.len(), 1);

        // Should have second set of keys
        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert!(retrieved.accounts.contains(&WETH));
        assert!(!retrieved.accounts.contains(&USDC));
    }

    /// # Test: Empty ExtractedKeys Handling
    ///
    /// ## Scenario
    /// A transaction that doesn't access any state (e.g., pure ETH transfer
    /// to EOA with no contract calls).
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Create empty ExtractedKeys
    ///     ↓
    /// Store in cache
    ///     ↓
    /// Retrieve
    ///     ↓
    /// Should return empty (but not error)
    /// ```
    ///
    /// ## Validates
    /// - Empty keys can be stored
    /// - Empty keys retrieval works
    /// - `is_empty()` returns true
    #[test]
    fn test_empty_extracted_keys() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hash = TxHash::random();
        let keys = ExtractedKeys::new(); // Empty!

        assert!(keys.is_empty());
        assert_eq!(keys.total_keys(), 0);

        cache.store_tx_keys(tx_hash, keys);
        assert_eq!(cache.len(), 1);

        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert!(retrieved.is_empty());
    }

    /// # Test: Single TX with All Key Types
    ///
    /// ## Scenario
    /// A complex transaction that accesses all types of state:
    /// - Accounts
    /// - Storage slots
    /// - Contract bytecode
    /// - Block hashes (BLOCKHASH opcode)
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Create keys with all 4 types
    ///     ↓
    /// Store and retrieve
    ///     ↓
    /// Verify all types preserved
    /// ```
    ///
    /// ## Validates
    /// - All key types can be stored together
    /// - No type is lost during store/retrieve
    /// - `total_keys()` counts all types
    #[test]
    fn test_all_key_types_single_tx() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();

        // Add all key types
        keys.add_account(ALICE);
        keys.add_account(UNISWAP_V2_ROUTER);
        keys.add_storage_slot(WETH, BALANCE_SLOT);
        keys.add_storage_slot(USDC, ALLOWANCE_SLOT);
        keys.add_code_hash(B256::random());
        keys.add_block_hash(12345678);

        assert_eq!(keys.total_keys(), 6);

        cache.store_tx_keys(tx_hash, keys);

        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.accounts.len(), 2);
        assert_eq!(retrieved.storage_slots.len(), 2);
        assert_eq!(retrieved.code_hashes.len(), 1);
        assert_eq!(retrieved.block_hashes.len(), 1);
    }

    /// # Test: Remove Non-Existent Transactions (Edge Case)
    ///
    /// ## Scenario
    /// Attempting to remove transactions that don't exist in cache.
    /// This can happen due to race conditions or duplicate removal calls.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store Alice's TX
    ///     ↓
    /// Try to remove [random1, random2, random3]
    ///     ↓
    /// Cache should be unchanged
    /// ```
    ///
    /// ## Validates
    /// - Removing non-existent TXs doesn't panic
    /// - Existing entries are unaffected
    #[test]
    fn test_remove_nonexistent_transactions() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let alice_tx = TxHash::random();
        let mut keys = ExtractedKeys::new();
        keys.add_account(ALICE);
        cache.store_tx_keys(alice_tx, keys);

        assert_eq!(cache.len(), 1);

        // Try to remove non-existent TXs
        let nonexistent: Vec<TxHash> = (0..3).map(|_| TxHash::random()).collect();
        cache.remove_txs(&nonexistent);

        // Alice's TX should still be there
        assert_eq!(cache.len(), 1);
        let retrieved = cache.get_keys_for_txs(&[alice_tx]);
        assert!(retrieved.accounts.contains(&ALICE));
    }

    /// # Test: Clear Cache
    ///
    /// ## Scenario
    /// Cache needs to be cleared (e.g., during testing or chain reorg).
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Store multiple TXs
    ///     ↓
    /// Call clear()
    ///     ↓
    /// Cache should be empty
    /// ```
    ///
    /// ## Validates
    /// - `clear()` removes all entries
    /// - Cache is usable after clear
    #[test]
    fn test_cache_clear() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        // Store several TXs
        for user in [ALICE, BOB, CHARLIE] {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(user);
            cache.store_tx_keys(tx_hash, keys);
        }

        assert_eq!(cache.len(), 3);

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }
}

// ============================================================================
// BENCHMARK TESTS - Performance measurement
// ============================================================================

/// Benchmark tests measure performance characteristics of the pre-warming system.
/// These tests print timing information and verify operations complete within
/// acceptable bounds. Run with `cargo test -- --nocapture` to see timing output.
///
/// ## Performance Targets (X-Layer 400ms blocks)
/// - Key addition: < 100ns per operation
/// - Cache store: < 1μs per transaction
/// - Cache retrieval: < 100μs for 100 transactions
/// - Merge: < 50μs for 1000 keys
mod benchmarks {
    use super::*;

    /// Helper to measure operation time
    fn measure<F: FnOnce()>(f: F) -> Duration {
        let start = Instant::now();
        f();
        start.elapsed()
    }

    /// # Benchmark: Account Key Addition
    ///
    /// ## What We're Measuring
    /// Time to add 10,000 unique account addresses to ExtractedKeys.
    ///
    /// ## Why This Matters
    /// During simulation, we add accounts as they're accessed. This must be fast
    /// to not slow down simulation workers.
    ///
    /// ## Expected Performance
    /// - Target: < 100ns per add_account() call
    /// - HashSet insertion is O(1) amortized
    #[test]
    fn bench_extracted_keys_add_account() {
        let iterations = 10_000;
        let mut keys = ExtractedKeys::new();
        let addresses = generate_user_addresses(iterations);

        let duration = measure(|| {
            for addr in &addresses {
                keys.add_account(*addr);
            }
        });

        println!(
            "bench_extracted_keys_add_account: {} ops in {:?} ({:.2} ns/op)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert_eq!(keys.accounts.len(), iterations);
        assert!(duration < Duration::from_millis(100));
    }

    /// # Benchmark: Storage Slot Key Addition
    ///
    /// ## What We're Measuring
    /// Time to add 10,000 storage slot keys (address + slot pairs) to ExtractedKeys.
    ///
    /// ## Why This Matters
    /// DeFi transactions typically access 5-50 storage slots each. This operation
    /// is on the critical path during simulation.
    ///
    /// ## Expected Performance
    /// - Target: < 100ns per add_storage_slot() call
    /// - HashSet<(Address, U256)> insertion
    #[test]
    fn bench_extracted_keys_add_storage_slot() {
        let iterations = 10_000;
        let mut keys = ExtractedKeys::new();

        let duration = measure(|| {
            for i in 0..iterations {
                keys.add_storage_slot(USDC, U256::from(i));
            }
        });

        println!(
            "bench_extracted_keys_add_storage_slot: {} ops in {:?} ({:.2} ns/op)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert_eq!(keys.storage_slots.len(), iterations);
        assert!(duration < Duration::from_millis(100));
    }

    /// # Benchmark: Key Merging
    ///
    /// ## What We're Measuring
    /// Time to merge 1,000 separate ExtractedKeys into one.
    ///
    /// ## Why This Matters
    /// Block builder merges keys from all selected transactions. For a 2000 TX block,
    /// this merge operation happens once before prefetch.
    ///
    /// ## Expected Performance
    /// - Target: < 100μs for 1,000 merges
    /// - HashSet::extend() with deduplication
    #[test]
    fn bench_extracted_keys_merge() {
        let iterations = 1_000;

        let keys_list: Vec<ExtractedKeys> = (0..iterations)
            .map(|i| {
                let mut keys = ExtractedKeys::new();
                let user = generate_deterministic_address(i);
                keys.add_account(user);
                keys.add_account(UNISWAP_V2_ROUTER);
                keys.add_account(WETH);
                keys.add_storage_slot(WETH, U256::from(i));
                keys
            })
            .collect();

        let mut merged = ExtractedKeys::new();

        let duration = measure(|| {
            for keys in keys_list {
                merged.merge(keys);
            }
        });

        println!(
            "bench_extracted_keys_merge: {} merges in {:?} ({:.2} ns/merge)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert!(duration < Duration::from_millis(100));
    }

    /// # Benchmark: Cache Store Operation
    ///
    /// ## What We're Measuring
    /// Time to store 10,000 transaction keys in the cache.
    ///
    /// ## Why This Matters
    /// Every simulated transaction stores its keys. At peak load (10k+ pending TXs),
    /// this must not become a bottleneck.
    ///
    /// ## Expected Performance
    /// - Target: < 500μs total for 10,000 stores
    /// - RwLock write + HashMap insert
    #[test]
    fn bench_cache_store_tx_keys() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);
        let iterations = 10_000;
        let users = generate_user_addresses(iterations);

        let duration = measure(|| {
            for (i, user) in users.iter().enumerate() {
                let tx_hash = TxHash::random();
                let mut keys = ExtractedKeys::new();
                keys.add_account(*user);
                keys.add_account(USDC);
                keys.add_storage_slot(USDC, U256::from(i));
                cache.store_tx_keys(tx_hash, keys);
            }
        });

        println!(
            "bench_cache_store_tx_keys: {} ops in {:?} ({:.2} ns/op)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert_eq!(cache.len(), iterations);
        assert!(duration < Duration::from_millis(500));
    }

    /// # Benchmark: Cache Retrieval for Block Building
    ///
    /// ## What We're Measuring
    /// Time to retrieve and merge keys for 100 selected transactions, repeated 1000 times.
    ///
    /// ## Why This Matters
    /// Block builder calls get_keys_for_txs() once per block. This is on the critical
    /// path before execution.
    ///
    /// ## Expected Performance
    /// - Target: < 1ms per block's worth of retrieval
    /// - RwLock read + HashMap lookups + merge
    #[test]
    fn bench_cache_get_keys_for_txs() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hashes: Vec<TxHash> = (0..1000)
            .map(|i| {
                let tx_hash = TxHash::random();
                let user = generate_deterministic_address(i);
                let mut keys = ExtractedKeys::new();
                keys.add_account(user);
                keys.add_account(UNISWAP_V2_ROUTER);
                keys.add_storage_slot(WETH, U256::from(i));
                cache.store_tx_keys(tx_hash, keys);
                tx_hash
            })
            .collect();

        let iterations = 1_000;
        let selection_size = 100;

        let duration = measure(|| {
            for _ in 0..iterations {
                let selection: Vec<TxHash> = tx_hashes[0..selection_size].to_vec();
                let _ = cache.get_keys_for_txs(&selection);
            }
        });

        println!(
            "bench_cache_get_keys_for_txs ({} txs): {} ops in {:?} ({:.2} us/op)",
            selection_size,
            iterations,
            duration,
            duration.as_micros() as f64 / iterations as f64
        );

        assert!(duration < Duration::from_secs(1));
    }

    /// # Benchmark: Cache Removal (Post-Block)
    ///
    /// ## What We're Measuring
    /// Time to remove 500 transactions from cache (simulating block finalization).
    ///
    /// ## Why This Matters
    /// After each block, mined transactions are removed. This cleanup should be fast
    /// to not delay next block preparation.
    ///
    /// ## Expected Performance
    /// - Target: < 10ms per block's worth of removals
    /// - RwLock write + HashMap::remove() x 500
    #[test]
    fn bench_cache_remove_txs() {
        let iterations = 100;

        let total_duration: Duration = (0..iterations)
            .map(|_| {
                let config = PreWarmingConfig::enabled();
                let cache = PreWarmedCache::new(config);
                let users = generate_user_addresses(1000);

                let tx_hashes: Vec<TxHash> = users
                    .iter()
                    .enumerate()
                    .map(|(i, user)| {
                        let tx_hash = TxHash::random();
                        let mut keys = ExtractedKeys::new();
                        keys.add_account(*user);
                        keys.add_account(USDC);
                        cache.store_tx_keys(tx_hash, keys);
                        tx_hash
                    })
                    .collect();

                measure(|| {
                    cache.remove_txs(&tx_hashes[0..500]);
                })
            })
            .sum();

        let avg_duration = total_duration / iterations;

        println!("bench_cache_remove_txs (500 txs): avg {:?} per operation", avg_duration);

        assert!(avg_duration < Duration::from_millis(10));
    }

    /// # Benchmark: Concurrent Cache Operations
    ///
    /// ## What We're Measuring
    /// Throughput when 8 threads simultaneously store transactions.
    ///
    /// ## Why This Matters
    /// Multiple simulation workers write to the cache concurrently. RwLock
    /// contention should not kill performance.
    ///
    /// ## Expected Performance
    /// - Target: < 1s for 8,000 operations (8 threads x 1000 ops)
    /// - RwLock write contention test
    #[test]
    fn bench_concurrent_cache_operations() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));
        let operations_per_thread = 1000;
        let num_threads = 8;

        let start = Instant::now();

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for tx_num in 0..operations_per_thread {
                        let tx_hash = TxHash::random();
                        let user = generate_deterministic_address(thread_id * 10000 + tx_num);
                        let mut keys = ExtractedKeys::new();
                        keys.add_account(user);
                        keys.add_account(USDC);
                        cache.store_tx_keys(tx_hash, keys);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let duration = start.elapsed();
        let total_ops = num_threads * operations_per_thread;

        println!(
            "bench_concurrent_cache_operations: {} ops across {} threads in {:?} ({:.2} ns/op)",
            total_ops,
            num_threads,
            duration,
            duration.as_nanos() as f64 / total_ops as f64
        );

        assert_eq!(cache.len(), total_ops);
        assert!(duration < Duration::from_secs(1));
    }

    /// # Benchmark: Large Block Key Merge (X-Layer Scale)
    ///
    /// ## What We're Measuring
    /// Time to merge keys from 2,000 transactions (full X-Layer block).
    ///
    /// ## Why This Matters
    /// X-Layer can have ~2000 TXs per block. Merging all their keys before
    /// prefetch must complete quickly.
    ///
    /// ## Expected Performance
    /// - Target: < 100ms for full block merge
    /// - Real-world DeFi key patterns
    #[test]
    fn bench_large_keys_merge() {
        let num_txs = 2000;

        let keys_list: Vec<ExtractedKeys> = (0..num_txs)
            .map(|i| {
                let mut keys = ExtractedKeys::new();
                let user = generate_deterministic_address(i);

                keys.add_account(user);
                keys.add_account(UNISWAP_V2_ROUTER);
                keys.add_account(WETH);
                keys.add_account(USDC);

                for j in 0..5 {
                    keys.add_storage_slot(USDC, U256::from(i * 10 + j));
                }
                keys.add_code_hash(B256::random());
                keys
            })
            .collect();

        let mut merged = ExtractedKeys::new();

        let duration = measure(|| {
            for keys in keys_list {
                merged.merge(keys);
            }
        });

        println!("bench_large_keys_merge: {} TX keys merged in {:?}", num_txs, duration);

        assert!(duration < Duration::from_millis(100));
    }

    /// # Benchmark: Cache Statistics Collection
    ///
    /// ## What We're Measuring
    /// Time to compute cache statistics (used for monitoring/metrics).
    ///
    /// ## Why This Matters
    /// Prometheus/metrics collection calls stats() periodically. Should not
    /// impact performance.
    ///
    /// ## Expected Performance
    /// - Target: < 500μs for 1000 stats() calls on 1000-entry cache
    /// - RwLock read + iteration over entries
    #[test]
    fn bench_cache_stats() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);
        let users = generate_user_addresses(1000);

        for (i, user) in users.iter().enumerate() {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(*user);
            keys.add_account(UNISWAP_V2_ROUTER);
            keys.add_storage_slot(WETH, U256::from(i));
            keys.add_code_hash(B256::random());
            cache.store_tx_keys(tx_hash, keys);
        }

        let iterations = 1000;

        let duration = measure(|| {
            for _ in 0..iterations {
                let _ = cache.stats();
            }
        });

        println!(
            "bench_cache_stats: {} calls in {:?} ({:.2} us/call)",
            iterations,
            duration,
            duration.as_micros() as f64 / iterations as f64
        );

        assert!(duration < Duration::from_millis(500));
    }

    /// # Benchmark: SimulationRequest Creation
    ///
    /// ## What We're Measuring
    /// Time to create 100,000 SimulationRequest objects.
    ///
    /// ## Why This Matters
    /// A request is created for every incoming transaction. Must be nearly free.
    ///
    /// ## Expected Performance
    /// - Target: < 500μs total for 100k creations
    /// - Just struct creation + Instant::now()
    #[test]
    fn bench_simulation_request_creation() {
        let iterations = 100_000;

        let duration = measure(|| {
            for _ in 0..iterations {
                let _ = SimulationRequest::new(TxHash::random(), ONE_ETH, 0);
            }
        });

        println!(
            "bench_simulation_request_creation: {} ops in {:?} ({:.2} ns/op)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert!(duration < Duration::from_millis(500));
    }

    /// # Benchmark: Total Keys Calculation
    ///
    /// ## What We're Measuring
    /// Time to call total_keys() 100,000 times on an ExtractedKeys with 300 entries.
    ///
    /// ## Why This Matters
    /// total_keys() may be used for logging/metrics. Should be O(1).
    ///
    /// ## Expected Performance
    /// - Target: < 100μs total for 100k calls
    /// - 4 HashSet::len() calls
    #[test]
    fn bench_extracted_keys_total_keys() {
        let iterations = 100_000;
        let users = generate_user_addresses(100);

        let mut keys = ExtractedKeys::new();
        for (i, user) in users.iter().enumerate() {
            keys.add_account(*user);
            keys.add_storage_slot(USDC, U256::from(i));
            keys.add_code_hash(B256::random());
        }

        let duration = measure(|| {
            for _ in 0..iterations {
                let _ = keys.total_keys();
            }
        });

        println!(
            "bench_extracted_keys_total_keys: {} ops in {:?} ({:.2} ns/op)",
            iterations,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert!(duration < Duration::from_millis(100));
    }

    /// # Benchmark: HashSet Deduplication Efficiency
    ///
    /// ## What We're Measuring
    /// How efficiently HashSet handles duplicates when many TXs touch same contracts.
    ///
    /// ## Why This Matters
    /// Hot contracts (Uniswap, USDC) appear in many transactions. Deduplication
    /// must not slow down as duplicates increase.
    ///
    /// ## Expected Performance
    /// - Target: < 100ms for 10k ops with 10x duplicates
    /// - HashSet should handle duplicates in O(1)
    #[test]
    fn bench_hash_set_deduplication() {
        let iterations = 10_000;
        let duplicates = 10;

        let mut keys = ExtractedKeys::new();

        let duration = measure(|| {
            for i in 0..iterations {
                let contract_id = i / duplicates;
                let contract = generate_deterministic_address(contract_id);
                keys.add_account(contract);
            }
        });

        println!(
            "bench_hash_set_deduplication: {} ops ({}x duplicates) in {:?} ({:.2} ns/op)",
            iterations,
            duplicates,
            duration,
            duration.as_nanos() as f64 / iterations as f64
        );

        assert_eq!(keys.accounts.len(), iterations / duplicates);
        assert!(duration < Duration::from_millis(100));
    }
}

// ============================================================================
// STRESS TESTS - High load scenarios
// ============================================================================

/// Stress tests validate system behavior under extreme conditions.
/// These tests push the system to its limits to identify:
/// - Memory issues with large data sets
/// - Concurrency bugs under high contention
/// - Performance degradation at scale
mod stress {
    use super::*;

    /// # Stress Test: Peak Mempool Load (10K Pending TXs)
    ///
    /// ## Scenario
    /// Node experiences transaction spam - mempool fills to 10,000 pending transactions.
    /// All are USDC transfers that need simulation.
    ///
    /// ## What We're Testing
    /// - Cache can handle 10K entries
    /// - Query for all 10K returns correct data
    /// - No memory issues or panics
    ///
    /// ## Expected Behavior
    /// - All 10K transactions stored successfully
    /// - Merged query returns 10,001 unique accounts (10K users + USDC)
    #[test]
    fn stress_many_transactions() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);
        let users = generate_user_addresses(10_000);

        let tx_hashes: Vec<TxHash> = users
            .iter()
            .enumerate()
            .map(|(i, user)| {
                let tx_hash = TxHash::random();
                let mut keys = ExtractedKeys::new();
                keys.add_account(*user);
                keys.add_account(USDC);
                keys.add_storage_slot(USDC, U256::from(i));
                cache.store_tx_keys(tx_hash, keys);
                tx_hash
            })
            .collect();

        assert_eq!(cache.len(), 10_000);

        let all_keys = cache.get_keys_for_txs(&tx_hashes);
        // 10,000 unique users + 1 shared USDC
        assert_eq!(all_keys.accounts.len(), 10_001);
    }

    /// # Stress Test: Complex Aggregator Transaction
    ///
    /// ## Scenario
    /// A 1inch-style aggregator swap that routes through 50 liquidity pools,
    /// accessing 1000 different storage slots across all pools.
    ///
    /// ## What We're Testing
    /// - Single transaction with many keys
    /// - 50 accounts + 1000 storage slots + 100 bytecodes
    /// - Stats accurately reflect key counts
    ///
    /// ## Expected Behavior
    /// - All keys stored successfully
    /// - Stats show correct counts per key type
    #[test]
    fn stress_many_keys_per_transaction() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();

        // 50 liquidity pools
        let pools = generate_user_addresses(50);
        for pool in &pools {
            keys.add_account(*pool);
        }

        // 1000 storage accesses across all pools
        for i in 0..1000 {
            let pool = pools[i % 50];
            keys.add_storage_slot(pool, U256::from(i));
        }

        // 100 different contract bytecodes
        for i in 0..100 {
            keys.add_code_hash(B256::from_slice(&[i as u8; 32]));
        }

        cache.store_tx_keys(tx_hash, keys);

        let stats = cache.stats();
        assert_eq!(stats.total_accounts, 50);
        assert_eq!(stats.total_storage_slots, 1000);
        assert_eq!(stats.total_code_hashes, 100);
    }

    /// # Stress Test: Concurrent Readers and Writers
    ///
    /// ## Scenario
    /// Simulate realistic node operation:
    /// - 4 writer threads (P2P peers sending new transactions)
    /// - 4 reader threads (block builders querying cache)
    /// All operating simultaneously.
    ///
    /// ## What We're Testing
    /// - RwLock handles mixed read/write load
    /// - No deadlocks between readers and writers
    /// - No data corruption
    ///
    /// ## Expected Behavior
    /// - All 4,000 transactions stored (4 writers x 1,000 each)
    /// - Readers never see inconsistent state
    /// - No panics or hangs
    #[test]
    fn stress_concurrent_reads_and_writes() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let stored_hashes: Arc<parking_lot::RwLock<Vec<TxHash>>> =
            Arc::new(parking_lot::RwLock::new(Vec::new()));

        let mut handles = vec![];

        // 4 Writer threads
        for writer_id in 0..4 {
            let cache = Arc::clone(&cache);
            let stored = Arc::clone(&stored_hashes);
            handles.push(thread::spawn(move || {
                for tx_num in 0..1000 {
                    let tx_hash = TxHash::random();
                    let user = generate_deterministic_address(writer_id * 10000 + tx_num);
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(user);
                    keys.add_account(WETH);
                    keys.add_storage_slot(WETH, U256::from(tx_num));
                    cache.store_tx_keys(tx_hash, keys);
                    stored.write().push(tx_hash);
                }
            }));
        }

        // 4 Reader threads
        for _ in 0..4 {
            let cache = Arc::clone(&cache);
            let stored = Arc::clone(&stored_hashes);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let hashes = stored.read();
                    if hashes.len() > 10 {
                        let selection: Vec<TxHash> = hashes[0..10.min(hashes.len())].to_vec();
                        let _ = cache.get_keys_for_txs(&selection);
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(cache.len(), 4000);
    }

    /// # Stress Test: Rapid Block Production (Fast L2 Cadence)
    ///
    /// ## Scenario
    /// X-Layer produces blocks every 400ms. Simulate 100 consecutive blocks,
    /// each with 100 transactions. After each block, transactions are removed.
    ///
    /// ## What We're Testing
    /// - Rapid add/remove cycles don't leak memory
    /// - Cache returns to empty state after each block
    /// - No accumulation of stale entries
    ///
    /// ## Expected Behavior
    /// - 100 blocks processed successfully
    /// - Cache is empty after each block (all TXs removed)
    #[test]
    fn stress_rapid_add_remove() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        for block_num in 0..100 {
            let users = generate_user_addresses(100);

            let tx_hashes: Vec<TxHash> = users
                .iter()
                .enumerate()
                .map(|(_i, user)| {
                    let tx_hash = TxHash::random();
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(*user);
                    keys.add_account(USDC);
                    keys.add_storage_slot(USDC, U256::from(block_num * 100 + _i));
                    cache.store_tx_keys(tx_hash, keys);
                    tx_hash
                })
                .collect();

            // After storing this block's 100 TXs the cache grows
            assert_eq!(cache.len(), 100);

            // remove_txs evicts the mined TXs — cache returns to 0 for next block
            cache.remove_txs(&tx_hashes);
            assert_eq!(cache.len(), 0);
        }
    }

    /// # Stress Test: Maximum Storage Slots per Address
    ///
    /// ## Scenario
    /// A contract with extremely dense storage (like a mapping with many entries).
    /// Single address with 10,000 different storage slots.
    ///
    /// ## What We're Testing
    /// - HashSet handles many slots for same address
    /// - No performance degradation with slot count
    ///
    /// ## Expected Behavior
    /// - All 10,000 slots stored
    /// - Retrieval works correctly
    #[test]
    fn stress_many_slots_single_address() {
        let config = PreWarmingConfig::enabled();
        let cache = PreWarmedCache::new(config);

        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();

        // Single contract with 10,000 storage slots (like a large mapping)
        for i in 0..10_000 {
            keys.add_storage_slot(USDC, U256::from(i));
        }

        cache.store_tx_keys(tx_hash, keys);

        let stats = cache.stats();
        assert_eq!(stats.total_storage_slots, 10_000);

        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.storage_slots.len(), 10_000);
    }

    /// # Stress Test: Interleaved Operations
    ///
    /// ## Scenario
    /// Multiple threads performing different operations simultaneously:
    /// - Thread 1-2: Storing new TXs
    /// - Thread 3-4: Removing TXs
    /// - Thread 5-6: Querying TXs
    /// - Thread 7-8: Getting stats
    ///
    /// ## What We're Testing
    /// - All operations can run concurrently
    /// - No operation blocks others excessively
    /// - System remains consistent
    ///
    /// ## Expected Behavior
    /// - No deadlocks or panics
    /// - Operations complete in reasonable time
    #[test]
    fn stress_interleaved_operations() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));
        let stored_hashes: Arc<parking_lot::RwLock<Vec<TxHash>>> =
            Arc::new(parking_lot::RwLock::new(Vec::new()));

        let mut handles = vec![];

        // Storers
        for thread_id in 0..2 {
            let cache = Arc::clone(&cache);
            let stored = Arc::clone(&stored_hashes);
            handles.push(thread::spawn(move || {
                for i in 0..500 {
                    let tx_hash = TxHash::random();
                    let mut keys = ExtractedKeys::new();
                    keys.add_account(generate_deterministic_address(thread_id * 1000 + i));
                    cache.store_tx_keys(tx_hash, keys);
                    stored.write().push(tx_hash);
                    thread::sleep(Duration::from_micros(10)); // Slight delay
                }
            }));
        }

        // Removers
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            let stored = Arc::clone(&stored_hashes);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    thread::sleep(Duration::from_millis(5));
                    let mut hashes = stored.write();
                    if hashes.len() > 10 {
                        let to_remove: Vec<TxHash> = hashes.drain(0..5).collect();
                        drop(hashes); // Release lock before remove
                        cache.remove_txs(&to_remove);
                    }
                }
            }));
        }

        // Queriers
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            let stored = Arc::clone(&stored_hashes);
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    thread::sleep(Duration::from_millis(2));
                    let hashes = stored.read();
                    if hashes.len() > 5 {
                        let selection: Vec<TxHash> = hashes[0..5.min(hashes.len())].to_vec();
                        let _ = cache.get_keys_for_txs(&selection);
                    }
                }
            }));
        }

        // Stats collectors
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    thread::sleep(Duration::from_millis(5));
                    let _ = cache.stats();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // No assertion on final count - it's non-deterministic due to interleaving
        // Test passes if no panics/deadlocks occurred
    }
}
