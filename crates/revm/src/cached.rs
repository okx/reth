//! Database adapters for payload building.
use alloc::sync::Arc;
use alloy_primitives::{
    map::{Entry, HashMap},
    Address, B256, U256,
};
use core::cell::RefCell;
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseRef};

/// A container type that caches reads from an underlying [`DatabaseRef`].
///
/// This is intended to be used in conjunction with `revm::db::State`
/// during payload building which repeatedly accesses the same data.
///
/// [`CachedReads::as_db_mut`] transforms this type into a [`Database`] implementation that uses
/// [`CachedReads`] as a caching layer for operations, and records any cache misses.
///
/// # Example
///
/// ```
/// use reth_revm::{cached::CachedReads, DatabaseRef, db::State};
///
/// fn build_payload<DB: DatabaseRef>(db: DB) {
///     let mut cached_reads = CachedReads::default();
///     let db = cached_reads.as_db_mut(db);
///     // this is `Database` and can be used to build a payload, it never commits to `CachedReads` or the underlying database, but all reads from the underlying database are cached in `CachedReads`.
///     // Subsequent payload build attempts can use cached reads and avoid hitting the underlying database.
///     // Note: `cached_reads` must outlive `db` to satisfy lifetime requirements.
///     let state = State::builder().with_database(db).build();
/// }
/// ```
#[derive(Clone, Default)]
pub struct CachedReads {
    /// Block state account with storage.
    pub accounts: HashMap<Address, CachedAccount>,
    /// Created contracts.
    pub contracts: HashMap<B256, Bytecode>,
    /// Block hash mapped to the block number.
    pub block_hashes: HashMap<u64, B256>,
    /// Optional callback for tracking cache hits (always available, set via set_metrics_callbacks)
    pub on_cache_hit: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Optional callback for tracking cache misses (always available, set via set_metrics_callbacks)
    pub on_cache_miss: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl core::fmt::Debug for CachedReads {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CachedReads")
            .field("accounts", &self.accounts)
            .field("contracts", &self.contracts)
            .field("block_hashes", &self.block_hashes)
            .finish()
    }
}

// === impl CachedReads ===

impl CachedReads {
    /// Gets a [`DatabaseRef`] that will cache reads from the given database.
    pub const fn as_db<DB>(&mut self, db: DB) -> CachedReadsDBRef<'_, DB> {
        self.as_db_mut(db).into_db()
    }

    /// Gets a mutable [`Database`] that will cache reads from the underlying database.
    pub const fn as_db_mut<DB>(&mut self, db: DB) -> CachedReadsDbMut<'_, DB> {
        CachedReadsDbMut { cached: self, db }
    }

    /// Sets the metrics callbacks for tracking cache hits/misses.
    ///
    /// Pass closures that will be called on cache hits and misses.
    pub fn set_metrics_callbacks(
        &mut self,
        on_hit: Arc<dyn Fn() + Send + Sync>,
        on_miss: Arc<dyn Fn() + Send + Sync>
    ) {
        self.on_cache_hit = Some(on_hit);
        self.on_cache_miss = Some(on_miss);
    }

    /// Inserts an account info into the cache.
    pub fn insert_account(
        &mut self,
        address: Address,
        info: AccountInfo,
        storage: HashMap<U256, U256>,
    ) {
        self.accounts.insert(address, CachedAccount { info: Some(info), storage });
    }

    /// Extends current cache with entries from another [`CachedReads`] instance.
    ///
    /// Note: It is expected that both instances are based on the exact same state.
    /// Callbacks are preserved from the current instance.
    pub fn extend(&mut self, other: Self) {
        self.accounts.extend(other.accounts);
        self.contracts.extend(other.contracts);
        self.block_hashes.extend(other.block_hashes);
        // Preserve callbacks from self, don't overwrite with other's callbacks
        if self.on_cache_hit.is_none() && other.on_cache_hit.is_some() {
            self.on_cache_hit = other.on_cache_hit;
        }
        if self.on_cache_miss.is_none() && other.on_cache_miss.is_some() {
            self.on_cache_miss = other.on_cache_miss;
        }
    }
}

/// A [Database] that caches reads inside [`CachedReads`].
///
/// The lifetime parameter `'a` is tied to the lifetime of the underlying [`CachedReads`] instance.
/// This ensures that the cache remains valid for the entire duration this wrapper is used.
/// The original [`CachedReads`] must outlive this wrapper to prevent use-after-free.
#[derive(Debug)]
pub struct CachedReadsDbMut<'a, DB> {
    /// The cache of reads.
    pub cached: &'a mut CachedReads,
    /// The underlying database.
    pub db: DB,
}

impl<'a, DB> CachedReadsDbMut<'a, DB> {
    /// Converts this [`Database`] implementation into a [`DatabaseRef`] that will still cache
    /// reads.
    pub const fn into_db(self) -> CachedReadsDBRef<'a, DB> {
        CachedReadsDBRef { inner: RefCell::new(self) }
    }

    /// Returns access to wrapped [`DatabaseRef`].
    pub const fn inner(&self) -> &DB {
        &self.db
    }
}

impl<DB, T> AsRef<T> for CachedReadsDbMut<'_, DB>
where
    DB: AsRef<T>,
{
    fn as_ref(&self) -> &T {
        self.inner().as_ref()
    }
}

impl<DB: DatabaseRef> Database for CachedReadsDbMut<'_, DB> {
    type Error = <DB as DatabaseRef>::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let basic = match self.cached.accounts.entry(address) {
            Entry::Occupied(entry) => {
                // Cache hit - data already in cache
                if let Some(ref on_hit) = self.cached.on_cache_hit {
                    on_hit();
                }
                entry.get().info.clone()
            }
            Entry::Vacant(entry) => {
                // Cache miss - need to fetch from underlying database
                if let Some(ref on_miss) = self.cached.on_cache_miss {
                    on_miss();
                }
                entry.insert(CachedAccount::new(self.db.basic_ref(address)?)).info.clone()
            }
        };
        Ok(basic)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let code = match self.cached.contracts.entry(code_hash) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry.insert(self.db.code_by_hash_ref(code_hash)?).clone(),
        };
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.cached.accounts.entry(address) {
            Entry::Occupied(mut acc_entry) => match acc_entry.get_mut().storage.entry(index) {
                Entry::Occupied(entry) => {
                    // Cache hit - storage slot already in cache
                    if let Some(ref on_hit) = self.cached.on_cache_hit {
                        on_hit();
                    }
                    Ok(*entry.get())
                }
                Entry::Vacant(entry) => {
                    // Cache miss - storage slot not in cache
                    if let Some(ref on_miss) = self.cached.on_cache_miss {
                        on_miss();
                    }
                    Ok(*entry.insert(self.db.storage_ref(address, index)?))
                }
            },
            Entry::Vacant(acc_entry) => {
                // Cache miss - account not in cache
                if let Some(ref on_miss) = self.cached.on_cache_miss {
                    on_miss();
                }
                // acc needs to be loaded for us to access slots.
                let info = self.db.basic_ref(address)?;
                let (account, value) = if info.is_some() {
                    let value = self.db.storage_ref(address, index)?;
                    let mut account = CachedAccount::new(info);
                    account.storage.insert(index, value);
                    (account, value)
                } else {
                    (CachedAccount::new(info), U256::ZERO)
                };
                acc_entry.insert(account);
                Ok(value)
            }
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        let hash = match self.cached.block_hashes.entry(number) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => *entry.insert(self.db.block_hash_ref(number)?),
        };
        Ok(hash)
    }
}

/// A [`DatabaseRef`] that caches reads inside [`CachedReads`].
///
/// This is intended to be used as the [`DatabaseRef`] for
/// `revm::db::State` for repeated payload build jobs.
///
/// The lifetime parameter `'a` matches the lifetime of the underlying [`CachedReadsDbMut`],
/// which in turn is tied to the [`CachedReads`] cache. [`RefCell`] is used here to provide
/// interior mutability for the [`DatabaseRef`] trait (which requires `&self`), while the
/// lifetime ensures the cache remains valid throughout the wrapper's usage.
#[derive(Debug)]
pub struct CachedReadsDBRef<'a, DB> {
    /// The inner cache reads db mut.
    pub inner: RefCell<CachedReadsDbMut<'a, DB>>,
}

impl<DB: DatabaseRef> DatabaseRef for CachedReadsDBRef<'_, DB> {
    type Error = <DB as DatabaseRef>::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.borrow_mut().basic(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.borrow_mut().code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.borrow_mut().storage(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.borrow_mut().block_hash(number)
    }
}

/// Cached account contains the account state with storage
/// but lacks the account status.
#[derive(Debug, Clone)]
pub struct CachedAccount {
    /// Account state.
    pub info: Option<AccountInfo>,
    /// Account's storage.
    pub storage: HashMap<U256, U256>,
}

impl CachedAccount {
    fn new(info: Option<AccountInfo>) -> Self {
        Self { info, storage: HashMap::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extend_with_two_cached_reads() {
        // Setup test data
        let hash1 = B256::from_slice(&[1u8; 32]);
        let hash2 = B256::from_slice(&[2u8; 32]);
        let address1 = Address::from_slice(&[1u8; 20]);
        let address2 = Address::from_slice(&[2u8; 20]);

        // Create primary cache
        let mut primary = {
            let mut cache = CachedReads::default();
            cache.accounts.insert(address1, CachedAccount::new(Some(AccountInfo::default())));
            cache.contracts.insert(hash1, Bytecode::default());
            cache.block_hashes.insert(1, hash1);
            cache
        };

        // Create additional cache
        let additional = {
            let mut cache = CachedReads::default();
            cache.accounts.insert(address2, CachedAccount::new(Some(AccountInfo::default())));
            cache.contracts.insert(hash2, Bytecode::default());
            cache.block_hashes.insert(2, hash2);
            cache
        };

        // Extending primary with additional cache
        primary.extend(additional);

        // Verify the combined state
        assert!(
            primary.accounts.len() == 2 &&
                primary.contracts.len() == 2 &&
                primary.block_hashes.len() == 2,
            "All maps should contain 2 entries"
        );

        // Verify specific entries
        assert!(
            primary.accounts.contains_key(&address1) &&
                primary.accounts.contains_key(&address2) &&
                primary.contracts.contains_key(&hash1) &&
                primary.contracts.contains_key(&hash2) &&
                primary.block_hashes.get(&1) == Some(&hash1) &&
                primary.block_hashes.get(&2) == Some(&hash2),
            "All expected entries should be present"
        );
    }
}
