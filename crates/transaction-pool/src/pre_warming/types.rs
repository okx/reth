//! Types for pre-warming simulation
//!
//! This module defines the core data structures for the pre-warming system:
//! - `SimulationRequest<T>`: Wrapper for simulation jobs sent to workers
//! - `ExtractedKeys`: Keys (not values!) extracted from transaction simulation
//!
//! These keys will be used in Phase 2 to batch-fetch actual data from MDBX
//! and pre-populate the existing CachedReads cache.
//!
//! ## Performance Optimizations
//! - Uses AHashSet for fast deduplication with SIMD-accelerated hashing
//! - Pre-allocates capacity for expected key counts

use ahash::AHashSet;
use alloy_primitives::{Address, TxHash, B256, U256};
use std::time::Instant;

/// Default capacity for accounts (typical ERC20 transfer touches 2-3 accounts)
const DEFAULT_ACCOUNTS_CAPACITY: usize = 8;
/// Default capacity for storage slots (typical ERC20 touches ~6 slots)
const DEFAULT_STORAGE_CAPACITY: usize = 16;

/// Request to simulate a transaction and extract accessed keys
#[derive(Debug, Clone)]
pub struct SimulationRequest<T> {
    /// Transaction hash
    pub tx_hash: TxHash,

    /// The transaction to simulate
    pub transaction: T,

    /// When the request was created
    pub timestamp: Instant,

    /// Effective gas tip (max_fee_per_gas) used for priority ordering.
    /// Higher tip = simulated first, matching executor's selection order.
    pub gas_tip: u128,
}

impl<T> SimulationRequest<T> {
    /// Create a new simulation request with explicit gas tip for priority ordering.
    pub fn new(tx_hash: TxHash, transaction: T, gas_tip: u128) -> Self {
        Self { tx_hash, transaction, timestamp: Instant::now(), gas_tip }
    }

    /// Age of this request
    pub fn age(&self) -> std::time::Duration {
        self.timestamp.elapsed()
    }
}

/// Keys extracted from transaction simulation
///
/// IMPORTANT: This stores KEYS ONLY, not actual data!
/// These keys map directly to database queries:
/// - `accounts` → basic_accounts(addresses)
/// - `storage_slots` → storage(address, slot)
/// - `code_hashes` → code_by_hash(code_hash)
/// - `block_hashes` → block_hash(number)
///
/// In Phase 2, these keys will be used to batch-fetch actual values from MDBX
/// and pre-populate the existing CachedReads cache before execution.
///
/// ## Performance Notes
/// - Uses AHashSet for O(1) deduplication with SIMD-accelerated hashing
/// - Pre-allocated capacity avoids rehashing during simulation
#[derive(Debug, Clone)]
pub struct ExtractedKeys {
    /// Accounts needing basic_account() query
    pub accounts: AHashSet<Address>,

    /// Storage slots needing storage() query
    /// Key: (address, slot)
    pub storage_slots: AHashSet<(Address, U256)>,

    /// Code hashes needing code_by_hash() query
    /// Note: Multiple addresses may share same code_hash (proxy contracts)
    pub code_hashes: AHashSet<B256>,

    /// Block numbers needing block_hash() query
    /// For BLOCKHASH opcode (rare)
    pub block_hashes: AHashSet<u64>,
}

impl ExtractedKeys {
    /// Create new empty ExtractedKeys with pre-allocated capacity
    #[inline]
    pub fn new() -> Self {
        Self {
            accounts: AHashSet::with_capacity(DEFAULT_ACCOUNTS_CAPACITY),
            storage_slots: AHashSet::with_capacity(DEFAULT_STORAGE_CAPACITY),
            code_hashes: AHashSet::with_capacity(4),
            block_hashes: AHashSet::with_capacity(2),
        }
    }

    /// Create ExtractedKeys with custom capacity for merging multiple transactions
    ///
    /// Use this when you know how many transactions will be merged.
    /// Avoids repeated re-allocations during merge operations.
    ///
    /// Typical values per transaction:
    /// - ETH transfer: 2 accounts, 0 storage
    /// - ERC20 transfer: 3 accounts, 3-6 storage slots
    #[inline]
    pub fn with_capacity_for_txs(num_txs: usize) -> Self {
        // Estimate: ~3 accounts and ~4 storage slots per transaction
        let accounts_cap = (num_txs * 3).max(DEFAULT_ACCOUNTS_CAPACITY);
        let storage_cap = (num_txs * 4).max(DEFAULT_STORAGE_CAPACITY);
        let code_cap = num_txs.max(4);

        Self {
            accounts: AHashSet::with_capacity(accounts_cap),
            storage_slots: AHashSet::with_capacity(storage_cap),
            code_hashes: AHashSet::with_capacity(code_cap),
            block_hashes: AHashSet::with_capacity(2),
        }
    }

    /// Add an account (will query basic_account in Phase 2)
    #[inline]
    pub fn add_account(&mut self, address: Address) {
        self.accounts.insert(address);
    }

    /// Add a storage slot (will query storage in Phase 2)
    #[inline]
    pub fn add_storage_slot(&mut self, address: Address, slot: U256) {
        self.storage_slots.insert((address, slot));
    }

    /// Add contract code by hash (will query code_by_hash in Phase 2)
    pub fn add_code_hash(&mut self, code_hash: B256) {
        self.code_hashes.insert(code_hash);
    }

    /// Add address with its code hash
    /// This adds both the account and its code hash
    pub fn add_address_with_code(&mut self, address: Address, code_hash: B256) {
        self.accounts.insert(address);
        self.code_hashes.insert(code_hash);
    }

    /// Add a block hash (will query block_hash in Phase 2)
    pub fn add_block_hash(&mut self, block_number: u64) {
        self.block_hashes.insert(block_number);
    }

    /// Add multiple accounts at once (batch insert)
    ///
    /// More efficient than calling add_account() in a loop.
    #[inline]
    pub fn add_accounts(&mut self, addresses: impl IntoIterator<Item = Address>) {
        self.accounts.extend(addresses);
    }

    /// Add multiple storage slots at once (batch insert)
    ///
    /// More efficient than calling add_storage_slot() in a loop.
    /// Use this when computing multiple slots for the same contract.
    #[inline]
    pub fn add_storage_slots(&mut self, slots: impl IntoIterator<Item = (Address, U256)>) {
        self.storage_slots.extend(slots);
    }

    /// Total number of keys
    #[inline]
    pub fn total_keys(&self) -> usize {
        self.accounts.len() +
            self.storage_slots.len() +
            self.code_hashes.len() +
            self.block_hashes.len()
    }

    /// Check if empty (no keys extracted)
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() &&
            self.storage_slots.is_empty() &&
            self.code_hashes.is_empty() &&
            self.block_hashes.is_empty()
    }

    /// Merge another ExtractedKeys into this one
    ///
    /// Used in Phase 2 to aggregate keys from multiple transactions
    /// before batch-fetching from MDBX.
    #[inline]
    pub fn merge(&mut self, other: ExtractedKeys) {
        self.accounts.extend(other.accounts);
        self.storage_slots.extend(other.storage_slots);
        self.code_hashes.extend(other.code_hashes);
        self.block_hashes.extend(other.block_hashes);
    }

    /// Merge from a reference without cloning the entire structure
    ///
    /// This is more efficient than `merge(other.clone())` because it only
    /// clones the individual keys that need to be inserted, not the entire
    /// HashSet structures.
    ///
    /// Skips empty sets and uses `reserve()` to avoid rehashing.
    #[inline]
    pub fn merge_ref(&mut self, other: &ExtractedKeys) {
        // Skip empty sets entirely (common for ETH transfers with no storage)
        if !other.accounts.is_empty() {
            self.accounts.reserve(other.accounts.len());
            self.accounts.extend(other.accounts.iter().copied());
        }
        if !other.storage_slots.is_empty() {
            self.storage_slots.reserve(other.storage_slots.len());
            self.storage_slots.extend(other.storage_slots.iter().copied());
        }
        if !other.code_hashes.is_empty() {
            self.code_hashes.reserve(other.code_hashes.len());
            self.code_hashes.extend(other.code_hashes.iter().copied());
        }
        if !other.block_hashes.is_empty() {
            self.block_hashes.reserve(other.block_hashes.len());
            self.block_hashes.extend(other.block_hashes.iter().copied());
        }
    }
}

impl Default for ExtractedKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================================
    // Basic Functionality Tests
    // ============================================================================

    #[test]
    fn test_simulation_request_creation() {
        let tx_hash = TxHash::random();
        let request = SimulationRequest::new(tx_hash, 42u64, 0);

        assert_eq!(request.tx_hash, tx_hash);
        assert_eq!(request.transaction, 42u64);
        assert!(request.age().as_millis() < 10);
    }

    #[test]
    fn test_simulation_request_age_increases() {
        let request = SimulationRequest::new(TxHash::random(), 42u64, 0);
        let age1 = request.age();

        std::thread::sleep(std::time::Duration::from_millis(10));
        let age2 = request.age();

        assert!(age2 > age1);
    }

    #[test]
    fn test_extracted_keys_default() {
        let keys = ExtractedKeys::default();
        assert!(keys.is_empty());
        assert_eq!(keys.total_keys(), 0);
    }

    #[test]
    fn test_extracted_keys_add_account() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        keys.add_account(addr);
        assert_eq!(keys.accounts.len(), 1);
        assert!(keys.accounts.contains(&addr));

        // Adding same address doesn't duplicate (HashSet)
        keys.add_account(addr);
        assert_eq!(keys.accounts.len(), 1);
    }

    #[test]
    fn test_extracted_keys_add_storage_slot() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();
        let slot = U256::from(42);

        keys.add_storage_slot(addr, slot);
        assert_eq!(keys.storage_slots.len(), 1);
        assert!(keys.storage_slots.contains(&(addr, slot)));

        // Adding same slot doesn't duplicate (HashSet)
        keys.add_storage_slot(addr, slot);
        assert_eq!(keys.storage_slots.len(), 1);
    }

    #[test]
    fn test_extracted_keys_add_code_hash() {
        let mut keys = ExtractedKeys::new();
        let code_hash = B256::random();

        keys.add_code_hash(code_hash);
        assert_eq!(keys.code_hashes.len(), 1);
        assert!(keys.code_hashes.contains(&code_hash));

        // Adding same code_hash doesn't duplicate
        keys.add_code_hash(code_hash);
        assert_eq!(keys.code_hashes.len(), 1);
    }

    #[test]
    fn test_address_with_code_hash() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();
        let code_hash = B256::random();

        keys.add_address_with_code(addr, code_hash);

        assert!(keys.accounts.contains(&addr));
        assert!(keys.code_hashes.contains(&code_hash));
    }

    // ============================================================================
    // Edge Cases - Boundary Values
    // ============================================================================

    #[test]
    fn test_zero_address() {
        let mut keys = ExtractedKeys::new();
        let zero_addr = Address::ZERO;

        keys.add_account(zero_addr);
        assert!(keys.accounts.contains(&zero_addr));
    }

    #[test]
    fn test_max_address() {
        let mut keys = ExtractedKeys::new();
        let max_addr = Address::from([0xff; 20]);

        keys.add_account(max_addr);
        assert!(keys.accounts.contains(&max_addr));
    }

    #[test]
    fn test_zero_storage_slot() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        keys.add_storage_slot(addr, U256::ZERO);
        assert!(keys.storage_slots.contains(&(addr, U256::ZERO)));
    }

    #[test]
    fn test_max_storage_slot() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();
        let max_slot = U256::MAX;

        keys.add_storage_slot(addr, max_slot);
        assert!(keys.storage_slots.contains(&(addr, max_slot)));
    }

    #[test]
    fn test_zero_code_hash() {
        let mut keys = ExtractedKeys::new();
        keys.add_code_hash(B256::ZERO);
        assert!(keys.code_hashes.contains(&B256::ZERO));
    }

    #[test]
    fn test_zero_block_number() {
        let mut keys = ExtractedKeys::new();
        keys.add_block_hash(0);
        assert!(keys.block_hashes.contains(&0));
    }

    #[test]
    fn test_max_block_number() {
        let mut keys = ExtractedKeys::new();
        keys.add_block_hash(u64::MAX);
        assert!(keys.block_hashes.contains(&u64::MAX));
    }

    // ============================================================================
    // Deduplication Tests
    // ============================================================================

    #[test]
    fn test_massive_duplicate_accounts() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Add same account 10,000 times
        for _ in 0..10_000 {
            keys.add_account(addr);
        }

        // Should only have 1 entry
        assert_eq!(keys.accounts.len(), 1);
    }

    #[test]
    fn test_massive_duplicate_storage_slots() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();
        let slot = U256::from(42);

        // Add same slot 10,000 times
        for _ in 0..10_000 {
            keys.add_storage_slot(addr, slot);
        }

        // Should only have 1 entry
        assert_eq!(keys.storage_slots.len(), 1);
    }

    #[test]
    fn test_mixed_duplicates_and_uniques() {
        let mut keys = ExtractedKeys::new();
        let addr1 = Address::from([1; 20]);
        let addr2 = Address::from([2; 20]);

        // Add pattern: 1, 1, 2, 1, 2, 2
        keys.add_account(addr1);
        keys.add_account(addr1);
        keys.add_account(addr2);
        keys.add_account(addr1);
        keys.add_account(addr2);
        keys.add_account(addr2);

        // Should only have 2 unique entries
        assert_eq!(keys.accounts.len(), 2);
    }

    // ============================================================================
    // Merge Tests
    // ============================================================================

    #[test]
    fn test_extracted_keys_merge() {
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));
        keys1.add_storage_slot(Address::from([1; 20]), U256::from(5));

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));
        keys2.add_code_hash(B256::from([3; 32]));

        keys1.merge(keys2);

        assert_eq!(keys1.accounts.len(), 2);
        assert_eq!(keys1.storage_slots.len(), 1);
        assert_eq!(keys1.code_hashes.len(), 1);
        assert_eq!(keys1.total_keys(), 4);
    }

    #[test]
    fn test_merge_with_duplicates() {
        let mut keys1 = ExtractedKeys::new();
        let addr = Address::from([1; 20]);
        keys1.add_account(addr);

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(addr); // Same address

        keys1.merge(keys2);

        // Should deduplicate
        assert_eq!(keys1.accounts.len(), 1);
    }

    #[test]
    fn test_merge_empty_into_populated() {
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::random());

        let keys2 = ExtractedKeys::new(); // Empty

        keys1.merge(keys2);

        // Should still have 1 account
        assert_eq!(keys1.accounts.len(), 1);
    }

    #[test]
    fn test_merge_populated_into_empty() {
        let mut keys1 = ExtractedKeys::new(); // Empty

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::random());

        keys1.merge(keys2);

        // Should now have 1 account
        assert_eq!(keys1.accounts.len(), 1);
    }

    #[test]
    fn test_merge_multiple_times() {
        let mut keys = ExtractedKeys::new();

        for i in 0..10 {
            let mut temp_keys = ExtractedKeys::new();
            temp_keys.add_account(Address::from([i; 20]));
            keys.merge(temp_keys);
        }

        assert_eq!(keys.accounts.len(), 10);
    }

    #[test]
    fn test_merge_all_field_types() {
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));
        keys1.add_storage_slot(Address::from([1; 20]), U256::from(1));
        keys1.add_code_hash(B256::from([1; 32]));
        keys1.add_block_hash(1);

        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));
        keys2.add_storage_slot(Address::from([2; 20]), U256::from(2));
        keys2.add_code_hash(B256::from([2; 32]));
        keys2.add_block_hash(2);

        keys1.merge(keys2);

        assert_eq!(keys1.accounts.len(), 2);
        assert_eq!(keys1.storage_slots.len(), 2);
        assert_eq!(keys1.code_hashes.len(), 2);
        assert_eq!(keys1.block_hashes.len(), 2);
        assert_eq!(keys1.total_keys(), 8);
    }

    // ============================================================================
    // Large Scale Tests
    // ============================================================================

    #[test]
    fn test_large_number_of_accounts() {
        let mut keys = ExtractedKeys::new();

        // Add 10,000 unique accounts
        for i in 0u64..10_000 {
            let mut bytes = [0u8; 20];
            bytes[0..8].copy_from_slice(&i.to_le_bytes());
            keys.add_account(Address::from(bytes));
        }

        assert_eq!(keys.accounts.len(), 10_000);
    }

    #[test]
    fn test_large_number_of_storage_slots() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Add 10,000 unique slots for same address
        for i in 0..10_000 {
            keys.add_storage_slot(addr, U256::from(i));
        }

        assert_eq!(keys.storage_slots.len(), 10_000);
    }

    #[test]
    fn test_storage_slots_multiple_addresses() {
        let mut keys = ExtractedKeys::new();

        // 100 addresses × 100 slots each = 10,000 total
        for i in 0..100 {
            let addr = Address::from([i as u8; 20]);
            for j in 0..100 {
                keys.add_storage_slot(addr, U256::from(j));
            }
        }

        assert_eq!(keys.storage_slots.len(), 10_000);
    }

    #[test]
    fn test_memory_efficiency_of_hashset() {
        // Verify HashSet doesn't waste memory on duplicates
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Add same address many times
        for _ in 0..1000 {
            keys.add_account(addr);
        }

        // Memory should be minimal (just one address + HashSet overhead)
        assert_eq!(keys.accounts.len(), 1);
        assert_eq!(keys.total_keys(), 1);
    }

    // ============================================================================
    // Same Address, Different Slots
    // ============================================================================

    #[test]
    fn test_same_address_multiple_slots() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        keys.add_storage_slot(addr, U256::from(0));
        keys.add_storage_slot(addr, U256::from(1));
        keys.add_storage_slot(addr, U256::from(2));

        assert_eq!(keys.storage_slots.len(), 3);
    }

    #[test]
    fn test_different_addresses_same_slot() {
        let mut keys = ExtractedKeys::new();
        let slot = U256::from(42);

        keys.add_storage_slot(Address::from([1; 20]), slot);
        keys.add_storage_slot(Address::from([2; 20]), slot);
        keys.add_storage_slot(Address::from([3; 20]), slot);

        assert_eq!(keys.storage_slots.len(), 3);
    }

    // ============================================================================
    // Proxy Pattern Tests (Same Code Hash, Multiple Addresses)
    // ============================================================================

    #[test]
    fn test_proxy_pattern_many_addresses_one_code() {
        let mut keys = ExtractedKeys::new();
        let code_hash = B256::random();

        // 1000 proxy contracts pointing to same implementation
        for i in 0u64..1000 {
            let mut bytes = [0u8; 20];
            bytes[0..8].copy_from_slice(&i.to_le_bytes());
            let addr = Address::from(bytes);
            keys.add_address_with_code(addr, code_hash);
        }

        // 1000 accounts, but only 1 code hash
        assert_eq!(keys.accounts.len(), 1000);
        assert_eq!(keys.code_hashes.len(), 1);
    }

    // ============================================================================
    // Empty/Default State Tests
    // ============================================================================

    #[test]
    fn test_extracted_keys_is_empty() {
        let keys = ExtractedKeys::new();
        assert!(keys.is_empty());
        assert_eq!(keys.total_keys(), 0);
    }

    #[test]
    fn test_empty_after_clearing_by_merge() {
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());

        // Replace with empty
        keys = ExtractedKeys::new();

        assert!(keys.is_empty());
    }

    // ============================================================================
    // Total Keys Calculation
    // ============================================================================

    #[test]
    fn test_extracted_keys_total() {
        let mut keys = ExtractedKeys::new();

        keys.add_account(Address::random());
        keys.add_account(Address::random());
        keys.add_storage_slot(Address::random(), U256::from(1));
        keys.add_code_hash(B256::random());
        keys.add_block_hash(123);

        assert_eq!(keys.total_keys(), 5);
        assert!(!keys.is_empty());
    }

    #[test]
    fn test_total_keys_with_duplicates() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::random();

        // Add duplicates
        keys.add_account(addr);
        keys.add_account(addr);
        keys.add_account(addr);

        // total_keys should reflect deduplicated count
        assert_eq!(keys.total_keys(), 1);
    }

    // ============================================================================
    // Clone and Equality Tests
    // ============================================================================

    #[test]
    fn test_extracted_keys_clone() {
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        keys.add_storage_slot(Address::random(), U256::from(42));

        let cloned = keys.clone();

        assert_eq!(keys.accounts.len(), cloned.accounts.len());
        assert_eq!(keys.storage_slots.len(), cloned.storage_slots.len());
        assert_eq!(keys.total_keys(), cloned.total_keys());
    }

    #[test]
    fn test_clone_independence() {
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));

        let mut keys2 = keys1.clone();
        keys2.add_account(Address::from([2; 20]));

        // Modifying clone shouldn't affect original
        assert_eq!(keys1.accounts.len(), 1);
        assert_eq!(keys2.accounts.len(), 2);
    }

    // ============================================================================
    // Stress Tests
    // ============================================================================

    #[test]
    fn test_extreme_merge_chain() {
        let mut base = ExtractedKeys::new();

        // Merge 1000 ExtractedKeys sequentially
        for i in 0u64..1000 {
            let mut temp = ExtractedKeys::new();
            let mut bytes = [0u8; 20];
            bytes[0..8].copy_from_slice(&i.to_le_bytes());
            temp.add_account(Address::from(bytes));
            base.merge(temp);
        }

        assert_eq!(base.accounts.len(), 1000);
    }

    #[test]
    fn test_mixed_operations_large_scale() {
        let mut keys = ExtractedKeys::new();

        // Mix of all operations
        for i in 0..100 {
            keys.add_account(Address::from([i; 20]));
            keys.add_storage_slot(Address::from([i; 20]), U256::from(i as u64));
            keys.add_code_hash(B256::from([i; 32]));
            keys.add_block_hash(i as u64);
        }

        assert_eq!(keys.accounts.len(), 100);
        assert_eq!(keys.storage_slots.len(), 100);
        assert_eq!(keys.code_hashes.len(), 100);
        assert_eq!(keys.block_hashes.len(), 100);
        assert_eq!(keys.total_keys(), 400);
    }
}
