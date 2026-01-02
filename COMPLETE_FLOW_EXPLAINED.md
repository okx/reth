# Complete Block Execution Flow: From Transaction Pool to State Root

**Where does it all start? How do transactions get processed? Let me show you the COMPLETE picture.**

---

## The Big Picture: Who Triggers What

```mermaid
sequenceDiagram
    participant Engine as Engine API<br/>engine/tree/src/engine/mod.rs
    participant Tree as EngineApiTreeHandler<br/>engine/tree/src/tree/mod.rs
    participant Payload as PayloadBuilder<br/>ethereum/payload/src/lib.rs
    participant Pool as TxPool<br/>transaction-pool/src/pool/txpool.rs
    participant Builder as BasicBlockBuilder<br/>evm/evm/src/execute.rs
    participant EVM as EVM (revm)
    participant State as State<DB><br/>revm-database/src/states/state.rs
    participant DB as TrieDB / StateProvider<br/>storage/provider/src/providers/state/latest.rs

    Note over Engine,DB: 🎯 TRIGGER POINT: Beacon chain requests new block
    
    Engine->>Tree: forkchoiceUpdated(head, payloadAttributes)<br/>EngineApiTreeHandler::on_forkchoice_updated
    Note over Engine: "Build me a block<br/>for timestamp X"
    
    Tree->>Payload: try_build(BuildArguments)<br/>EthereumPayloadBuilder::try_build L88
    Note over Tree: Start payload building
    
    Payload->>Pool: best_transactions_with_attributes(basefee, blob_gas)<br/>TxPool::best_transactions_with_attributes L381
    Note over Pool: "Give me the best txs<br/>sorted by priority fee"
    
    Pool-->>Payload: Box<dyn BestTransactions><br/>Iterator over ValidPoolTransaction
    Note over Pool: From PendingPool + BaseFeePool + BlobPool<br/>ordered by effective_tip_per_gas
    
    loop For each transaction (L270)
        Payload->>Builder: execute_transaction(tx)<br/>BasicBlockBuilder::execute_transaction L367
        Builder->>EVM: Executor::execute_transaction_with_commit_condition L491
        EVM->>State: Update balances, nonces, storage<br/>State accumulates in TransitionState
        Note over State: Changes stored in TransitionState<br/>HashMap<Address, TransitionAccount>
    end
    
    Note over Builder: ✅ All transactions executed (L355)
    
    Builder->>State: db.merge_transitions(BundleRetention::Reverts)<br/>State::merge_transitions L179
    Note over State: TransitionState → BundleState<br/>apply_transitions_and_create_reverts
    
    Builder->>Builder: Convert BundleState → PlainPostState L527-554
    Note over Builder: U256 storage keys → B256 keys<br/>Prepare for TrieDB
    
    Builder->>DB: state.state_root_with_updates_triedb(plain_state)<br/>StateRootProvider::state_root_with_updates_triedb L109
    Note over DB: TrieDB computes Merkle root<br/>342ms on 3G dataset
    
    DB-->>Builder: (state_root: B256, trie_updates)
    
    Builder->>Builder: assembler.assemble_block(state_root, ...) L585
    Note over Builder: Put state_root in block header
    
    Builder-->>Payload: BlockBuilderOutcome L592
    Payload-->>Tree: BuildOutcome::Better { payload }
    Tree-->>Engine: payload_id
    
    Note over Engine: Later: getPayload(payloadId)
```

---

## 1. The Trigger Point: Where It All Begins

### Location: Engine API receives forkchoiceUpdated

```rust
// When consensus layer (beacon chain) sends forkchoiceUpdated with payload attributes
EngineAPI::forkchoiceUpdated(
    forkchoice_state,  // Current head, safe, finalized blocks
    payload_attributes, // timestamp, prev_randao, suggested_fee_recipient, withdrawals
)
```

**Who calls this?**
- External: Consensus client (Lighthouse, Prysm, etc.)
- Internal: Can also be triggered by local miner for testing

**What happens?**
1. Engine validates the forkchoice state
2. If `payload_attributes` is present → **Start building a new block**
3. Creates a payload job and returns `payload_id`

---

## 2. Transaction Selection: Where Transactions Come From

### The Transaction Pool (Mempool)

```mermaid
flowchart TB
    subgraph Network["🌐 Network Layer"]
        P2P[P2P Gossip<br/>Receive txs from peers]
        RPC[RPC<br/>eth_sendRawTransaction]
    end
    
    subgraph Pool["📦 Transaction Pool (Mempool)"]
        Validate[Validation<br/>• Signature check<br/>• Nonce check<br/>• Balance check]
        
        subgraph Pending["Pending Transactions"]
            ByNonce[Organized by Address+Nonce]
            BySender[Sender → List of txs]
        end
        
        subgraph Queued["Queued Transactions"]
            Future[Future nonce txs<br/>waiting for gaps]
        end
        
        PriorityQueue[Priority Queue<br/>Sorted by effective_tip]
    end
    
    subgraph PayloadBuilder["🏗️ Payload Builder"]
        Request[Request: best_transactions_with_attributes<br/>basefee, blob_gas_price]
        Iterator[BestTransactions Iterator<br/>Yields txs in priority order]
    end
    
    P2P --> Validate
    RPC --> Validate
    Validate --> Pending
    Validate --> Queued
    
    Pending --> PriorityQueue
    
    Request --> PriorityQueue
    PriorityQueue --> Iterator
    
    style Network fill:#e3f2fd
    style Pool fill:#fff3e0
    style PayloadBuilder fill:#f3e5f5
```

### How Transactions Get Selected

```rust
// From crates/ethereum/payload/src/lib.rs

pub fn default_ethereum_payload(...) {
    // 1. Get best transactions from pool
    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,           // Base fee of the block
        blob_gasprice,      // Blob gas price (for EIP-4844)
    ));
    
    // 2. Iterate through transactions in order of profitability
    while let Some(pool_tx) = best_txs.next() {
        // Check gas limit
        if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
            best_txs.mark_invalid(&pool_tx, ExceedsGasLimit);
            continue;  // Skip this tx and its descendants
        }
        
        // Check blob count (EIP-4844)
        if blob_count + tx_blob_count > max_blob_count {
            best_txs.mark_invalid(&pool_tx, TooManyBlobs);
            continue;
        }
        
        // 3. Execute transaction
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(error) => {
                // Mark invalid and skip descendants
                best_txs.mark_invalid(&pool_tx, error);
                continue;
            }
        };
        
        cumulative_gas_used += gas_used;
        total_fees += miner_fee * gas_used;
    }
    
    // 4. Finish block building
    let BlockBuilderOutcome { block, execution_result, .. } = 
        builder.finish(&state_provider)?;
}
```

**Key Points:**
- Transactions are sorted by `effective_tip_per_gas` (priority fee)
- Builder tries to maximize total fees while staying under gas limit
- Invalid transactions and their descendants are skipped
- Continues until gas limit reached or no more profitable transactions

---

## 3. Transaction Execution: What Happens Inside the EVM

### Location: `crates/evm/evm/src/execute.rs`

```mermaid
flowchart TB
    Start[execute_transaction called] --> LoadAccount[Load account from State]
    
    LoadAccount --> CheckNonce{Nonce valid?}
    CheckNonce -->|No| Reject[❌ Reject tx]
    CheckNonce -->|Yes| CheckBalance
    
    CheckBalance{Balance ≥ value + gas?}
    CheckBalance -->|No| Reject
    CheckBalance -->|Yes| DeductGas[Deduct gas prepayment]
    
    DeductGas --> ExecuteEVM[🔥 Execute in EVM]
    
    subgraph EVMExecution["EVM Execution"]
        Decode[Decode transaction]
        SetupEnv[Setup EVM environment]
        RunCode[Run bytecode]
        
        subgraph StateChanges["State Changes (in TransitionState)"]
            Balance[Update balances]
            Nonce[Increment nonce]
            Storage[Modify storage]
            Code[Deploy code if contract creation]
            Destroy[Self-destruct if called]
        end
        
        Decode --> SetupEnv --> RunCode --> StateChanges
    end
    
    ExecuteEVM --> CheckResult{Success?}
    
    CheckResult -->|Revert| Rollback[Rollback state changes<br/>Keep gas deduction]
    CheckResult -->|Success| CommitChanges[Keep all state changes]
    
    Rollback --> RefundGas[Refund unused gas]
    CommitChanges --> RefundGas
    
    RefundGas --> AddToBundle[Add to TransitionState]
    AddToBundle --> Return[✅ Return gas_used]
    
    style EVMExecution fill:#e3f2fd
    style StateChanges fill:#fff3e0
```

### The State Object (from revm)

```rust
// From revm State structure
pub struct State<DB> {
    /// Cached state: address → account info + storage
    cache: CacheState,
    
    /// Transition state: changes from current block's transactions
    /// This is what accumulates during execution!
    transition_state: Option<TransitionState>,
    
    /// Bundle state: final committed changes
    /// Created by merge_transitions()
    bundle_state: BundleState,
    
    /// Database access
    database: DB,
}

pub struct TransitionState {
    /// All account changes from transactions
    /// Maps: Address → BundleAccount (with storage changes)
    transitions: HashMap<Address, TransitionAccount>,
}

pub struct TransitionAccount {
    /// Account info changes
    info: Option<AccountInfo>,
    
    /// Status: Created | Touched | Destroyed
    status: AccountStatus,
    
    /// Storage slot changes
    /// Maps: U256 slot → StorageSlot
    storage: HashMap<U256, StorageSlot>,
    
    /// Previous storage values (for reverts)
    previous_storage: HashMap<U256, U256>,
}
```

**During transaction execution:**
1. Each `SSTORE` adds to `TransitionAccount.storage`
2. Balance changes update `TransitionAccount.info.balance`
3. Nonce increments update `TransitionAccount.info.nonce`
4. Contract deployment sets `TransitionAccount.info.code_hash`

---

## 4. merge_transitions: The Critical Transformation

### Location: `revm::State::merge_transitions()`

This is where individual transaction changes become a unified block state!

```mermaid
flowchart LR
    subgraph Before["Before merge_transitions"]
        TS[TransitionState<br/>Transaction 1 changes<br/>Transaction 2 changes<br/>Transaction 3 changes<br/>...<br/>All separate]
    end
    
    subgraph During["merge_transitions BundleRetention::Reverts"]
        Merge[Merge all transitions]
        CreateReverts[Create revert records<br/>for each change]
        Consolidate[Consolidate duplicates<br/>Keep final state]
    end
    
    subgraph After["After merge_transitions"]
        BS[BundleState<br/>✅ Final state per address<br/>✅ Revert records saved<br/>✅ Ready for state root]
    end
    
    Before --> During --> After
    
    style Before fill:#fff3e0
    style During fill:#e3f2fd
    style After fill:#c8e6c9
```

### What merge_transitions Does

```rust
// From revm-database/src/states/state.rs
impl<DB> State<DB> {
    pub fn merge_transitions(&mut self, retention: BundleRetention) {
        if let Some(transition_state) = self.transition_state.take() {
            // Apply transitions and create reverts
            self.bundle_state.apply_transitions_and_create_reverts(
                transition_state,
                retention  // BundleRetention::Reverts
            );
        }
    }
}

// What apply_transitions_and_create_reverts does:
// 1. For each address in TransitionState:
//    a. Merge storage changes (last write wins)
//    b. Merge balance changes
//    c. Track account status (created/destroyed)
//    d. Create revert record (for re-orgs)
//
// 2. Build final BundleState:
//    state: HashMap<Address, BundleAccount> {
//        0x1234...: BundleAccount {
//            info: Some(Account { nonce: 5, balance: 100 ETH, ... }),
//            status: Touched,
//            storage: {
//                U256(10): StorageSlot {
//                    previous_value: U256(0),
//                    present_value: U256(42)  ← FINAL value after all txs
//                }
//            }
//        }
//    }
```

**Example: Multiple Transactions Changing Same Storage**

```rust
// Initial state: account 0xABCD has storage[slot 5] = 100

// Transaction 1: Sets storage[slot 5] = 200
TransitionState after tx1: {
    0xABCD: { storage: { 5: 200 } }
}

// Transaction 2: Sets storage[slot 5] = 300
TransitionState after tx2: {
    0xABCD: { storage: { 5: 200 } },  // tx1's change
    0xABCD: { storage: { 5: 300 } },  // tx2's change (separate!)
}

// merge_transitions() called:
BundleState = {
    0xABCD: BundleAccount {
        storage: {
            5: StorageSlot {
                previous_value: 100,   // Original value
                present_value: 300     // Final value (last write wins)
            }
        }
    }
}

// Revert record created:
Reverts[block_num] = {
    0xABCD: { storage: { 5: 100 } }  // Can revert to this if needed
}
```

---

## 5. State Root Calculation: The Final Step

### When It Happens

```rust
// In BasicBlockBuilder::finish()
fn finish(self, state: impl StateProvider) -> Result<BlockBuilderOutcome> {
    // Get execution result from EVM
    let (evm, result) = self.executor.finish()?;
    let (db, evm_env) = evm.finish();
    
    // 🔥 THIS IS THE CRITICAL CALL 🔥
    db.merge_transitions(BundleRetention::Reverts);
    
    // Now BundleState has final state of all accounts
    // db.bundle_state contains the complete picture
    
    // Convert BundleState → PlainPostState (for TrieDB)
    let mut plain_state = PlainPostState::default();
    for (address, bundle_account) in db.bundle_state.state() {
        // Convert each account
        plain_state.accounts.insert(*address, account);
        
        // Convert storage (U256 keys → B256 keys)
        for (slot, storage_slot) in &bundle_account.storage {
            let slot_b256 = B256::from_slice(&slot.to_be_bytes::<32>());
            storage_map.insert(slot_b256, storage_slot.present_value);
        }
        plain_state.storages.insert(*address, storage_map);
    }
    
    // Calculate state root using TrieDB
    let (state_root, trie_updates) = 
        state.state_root_with_updates_triedb(plain_state)?;
    
    // Assemble final block with state_root in header
    let block = self.assembler.assemble_block(BlockAssemblerInput {
        state_root,  // ← Goes into block header
        transactions,
        ...
    })?;
    
    Ok(BlockBuilderOutcome { block, execution_result, trie_updates })
}
```

---

## 6. Complete Flow with Line Numbers

```mermaid
sequenceDiagram
    autonumber
    
    participant Beacon as Consensus Client<br/>(External: Lighthouse/Prysm)
    participant Engine as EngineApiTreeHandler<br/>crates/engine/tree/src/tree/mod.rs
    participant Payload as EthereumPayloadBuilder<br/>crates/ethereum/payload/src/lib.rs:142
    participant Pool as TxPool<br/>crates/transaction-pool/src/pool/txpool.rs:381
    participant Builder as BasicBlockBuilder<br/>crates/evm/evm/src/execute.rs:515
    participant EVM as EVM (revm)<br/>execute_transaction_with_commit_condition
    participant State as State<DB><br/>revm-database/src/states/state.rs:179
    participant TrieDB as TriedbProvider<br/>crates/storage/provider/src/providers/state/latest.rs:109
    
    Note over Beacon: Time for new block!
    
    Beacon->>Engine: forkchoiceUpdated(head, attributes)
    Engine->>Payload: try_build(BuildArguments)<br/>default_ethereum_payload()
    
    Note over Payload: lib.rs:142 - default_ethereum_payload<br/>lib.rs:169 - Create StateProviderDatabase<br/>lib.rs:172 - State::builder().build()
    
    Payload->>Pool: pool.best_transactions_with_attributes(basefee, blob_gas)<br/>TxPool::best_transactions_with_attributes()
    Pool-->>Payload: Box<dyn BestTransactions><br/>BestTransactions iterator
    
    Note over Payload: lib.rs:214 - while let Some(pool_tx) = best_txs.next()
    
    loop For each transaction (lib.rs:214-343)
        Payload->>Builder: builder.execute_transaction(tx.clone())<br/>lib.rs:296
        Note over Builder: BasicBlockBuilder::execute_transaction<br/>execute.rs:367
        
        Builder->>EVM: executor.execute_transaction_with_commit_condition<br/>execute.rs:491-505
        EVM->>State: State<DB>::load_cache_account(address)<br/>state.rs:190
        EVM->>State: CacheAccount update balance/nonce<br/>Stored in TransitionState
        EVM->>State: StorageSlot changes added<br/>transition_state.transitions
        EVM->>State: Apply state changes<br/>AccountStatus: Touched/Created/Destroyed
        
        Note over State: All changes in TransitionState<br/>HashMap<Address, TransitionAccount>
        
        State-->>EVM: ExecutionResult { gas_used, ... }
        EVM-->>Builder: Ok(gas_used)
        Builder-->>Payload: gas_used: u64
        
        Note over Payload: lib.rs:341 - cumulative_gas_used += gas_used<br/>lib.rs:344 - total_fees += miner_fee × gas_used
    end
    
    Note over Payload: lib.rs:355 - Loop complete<br/>All profitable txs executed
    
    Payload->>Builder: builder.finish(&state_provider)<br/>lib.rs:358 calls BasicBlockBuilder::finish
    Note over Builder: execute.rs:515 - BasicBlockBuilder::finish()
    
    Builder->>State: db.merge_transitions(BundleRetention::Reverts)<br/>execute.rs:518
    Note over State: state.rs:179 - State::merge_transitions<br/>state.rs:180-183 - apply_transitions_and_create_reverts<br/>TransitionState → BundleState
    
    Note over Builder: execute.rs:527-554<br/>for (address, bundle_account) in db.bundle_state.state()<br/>Convert U256 storage keys → B256<br/>Build PlainPostState
    
    Builder->>TrieDB: state.state_root_with_updates_triedb(plain_state)<br/>execute.rs:557-558
    Note over TrieDB: latest.rs:109 - state_root_with_updates_triedb<br/>TriedbProvider::compute_root_with_overlay<br/>342ms vs 952ms (MDBX)
    
    TrieDB-->>Builder: (triedb_state_root: B256, trie_updates)<br/>execute.rs:565-566
    
    Note over Builder: execute.rs:585-592<br/>assembler.assemble_block(state_root)<br/>Create RecoveredBlock
    
    Builder-->>Payload: BlockBuilderOutcome { block, execution_result, trie_updates }<br/>execute.rs:596
    Payload-->>Engine: BuildOutcome::Better { payload, cached_reads }<br/>lib.rs:374
    Engine-->>Beacon: ForkChoiceUpdated { payload_id, ... }
    
    Note over Beacon: Later: getPayload(payload_id)
```

---

## 7. Data Structures at Each Stage

### Stage 1: Transactions in Pool

```rust
ValidPoolTransaction {
    transaction: TransactionSigned,  // Signed tx with signature
    transaction_id: TransactionId,   // Unique ID
    propagate: bool,                 // Should broadcast to peers?
    timestamp: Instant,              // When added to pool
    origin: TransactionOrigin,       // Local or External
    submission_id: u64,              // Ordering
}
```

### Stage 2: During Execution (TransitionState)

```rust
// Inside State<DB> during execution
TransitionState {
    transitions: HashMap<Address, TransitionAccount> {
        0x1234...: TransitionAccount {
            info: Some(AccountInfo {
                nonce: 5,
                balance: 100_000_000_000_000_000_000,  // 100 ETH
                code_hash: Some(0x5678...),
            }),
            status: Touched,
            storage: {
                U256(10): StorageSlot {
                    previous_or_original_value: U256(0),
                    present_value: U256(42),
                }
            }
        }
    }
}
```

### Stage 3: After merge_transitions (BundleState)

```rust
BundleState {
    state: HashMap<Address, BundleAccount> {
        0x1234...: BundleAccount {
            info: Some(AccountInfo { ... }),  // Final account info
            status: Touched,
            storage: HashMap<U256, StorageSlot> {
                U256(10): StorageSlot {
                    previous_or_original_value: U256(0),   // Before block
                    present_value: U256(42)                 // After block
                }
            }
        }
    },
    reverts: Vec<Vec<(Address, AccountRevert)>>,  // For re-org handling
}
```

### Stage 4: For State Root (PlainPostState)

```rust
PlainPostState {
    accounts: HashMap<Address, Option<Account>> {
        0x1234...: Some(Account {
            nonce: 5,
            balance: U256::from(100_000_000_000_000_000_000u128),
            bytecode_hash: Some(0x5678...),
        })
    },
    storages: HashMap<Address, HashMap<B256, U256>> {
        0x1234...: {
            B256(keccak256(10)): U256(42)  // Converted to B256 key!
        }
    }
}
```

### Stage 5: Final Block

```rust
Block {
    header: Header {
        parent_hash: 0xabcd...,
        number: 12345,
        state_root: 0xef01...,  // ← Calculated state root!
        transactions_root: 0x2345...,
        receipts_root: 0x6789...,
        gas_used: 21_000_000,
        timestamp: 1704240000,
        ...
    },
    body: BlockBody {
        transactions: Vec<TransactionSigned>,
        ommers: Vec<Header>,
        withdrawals: Option<Withdrawals>,
    }
}
```

---

## 8. Key Questions Answered

### Q: Where do transactions come from?

**A:** Transaction Pool (Mempool)
- Received from: P2P network (gossip) or RPC (eth_sendRawTransaction)
- Validated: Signature, nonce, balance checks
- Organized: By sender address and nonce
- Sorted: By effective_tip_per_gas (profitability)

### Q: How does payload builder pick transactions?

**A:** `best_transactions_with_attributes(basefee, blob_gas)`
- Returns an iterator that yields transactions in order of profitability
- Higher priority fee = selected first
- Continues until gas limit or no profitable txs remain

### Q: What is merge_transitions doing?

**A:** Consolidates all transaction changes into final block state
1. Takes `TransitionState` (individual tx changes)
2. Merges them into `BundleState` (final per-account state)
3. Creates revert records (for handling re-orgs)
4. Last write wins for conflicting changes

### Q: When is state root calculated?

**A:** After all transactions executed and merged
1. Execute all transactions → TransitionState
2. `merge_transitions()` → BundleState
3. Convert BundleState → PlainPostState
4. `state_root_with_updates_triedb()` → state_root
5. Assemble block with state_root in header

### Q: Why convert BundleState to PlainPostState?

**A:** Different key formats
- `BundleState`: Uses `U256` storage keys (EVM format)
- `PlainPostState`: Uses `B256` storage keys (32-byte format)
- TrieDB needs plain addresses (not hashed yet)
- Hashing happens inside TrieDB

---

## 9. The Complete Call Stack

```
1. Engine API receives forkchoiceUpdated()
   └─> crates/engine/tree/src/tree/mod.rs
       └─> EngineApiTreeHandler::on_forkchoice_updated()

2. Create payload building job
   └─> crates/payload/builder/src/service.rs
       └─> PayloadBuilderService::poll()

3. PayloadBuilder::try_build()
   └─> crates/ethereum/payload/src/lib.rs:88
       └─> impl PayloadBuilder for EthereumPayloadBuilder

4. default_ethereum_payload()
   └─> crates/ethereum/payload/src/lib.rs:142
       └─> Setup: StateProviderDatabase (L169)
       └─> Create: State::builder().with_bundle_update().build() (L172)
   
5. Get best transactions from pool
   └─> pool.best_transactions_with_attributes(BestTransactionsAttributes)
       └─> crates/transaction-pool/src/lib.rs:595
           └─> Pool::best_transactions_with_attributes()
               └─> crates/transaction-pool/src/pool/mod.rs:766
                   └─> TxPool::best_transactions_with_attributes()
                       └─> crates/transaction-pool/src/pool/txpool.rs:381
   
6. For each transaction (L214-343 loop):
   builder.execute_transaction(tx)
   └─> crates/evm/evm/src/execute.rs:367
       └─> BasicBlockBuilder::execute_transaction()
           └─> execute.rs:491
               └─> executor.execute_transaction_with_commit_condition()
                   └─> Calls revm EVM::transact()
                       └─> State<DB> accumulates changes in TransitionState
   
7. EVM execution (revm internals)
   └─> revm::Evm::transact()
       └─> State::load_cache_account() - state.rs:190
       └─> CacheAccount modifications stored in TransitionState
       └─> transition_state.transitions: HashMap<Address, TransitionAccount>
   
8. After all transactions (L355):
   db.merge_transitions(BundleRetention::Reverts)
   └─> .cargo/registry/.../revm-database-9.0.5/src/states/state.rs:179
       └─> State::merge_transitions()
           └─> bundle_state.apply_transitions_and_create_reverts() - L182
               └─> Creates final BundleState with revert records
   
9. Convert to PlainPostState
   └─> crates/evm/evm/src/execute.rs:527-554
       └─> Loop: for (address, bundle_account) in db.bundle_state.state()
           └─> Convert U256 storage keys → B256 keys
           └─> Build PlainPostState { accounts, storages }
   
10. Calculate state root
    state.state_root_with_updates_triedb(plain_state)
    └─> crates/evm/evm/src/execute.rs:557-558
        └─> crates/storage/provider/src/providers/state/latest.rs:109
            └─> LatestStateProviderRef::state_root_with_updates_triedb()
                └─> Get TriedbProvider and begin read transaction
                └─> Build OverlayStateMut from PlainPostState
                └─> tx.compute_root_with_overlay(overlay)
                └─> Returns (root: B256, TrieUpdates)
    
11. Assemble block
    assembler.assemble_block(state_root, ...)
    └─> crates/evm/evm/src/execute.rs:585-592
        └─> BlockAssemblerInput with state_root in header
        └─> Returns BlockBuilderOutcome
    
12. Return built payload
    └─> crates/ethereum/payload/src/lib.rs:374
        └─> BuildOutcome::Better { payload, cached_reads }
            └─> Engine API returns ForkChoiceUpdated with payload_id
```

---

## Summary: The Journey of a Transaction

```mermaid
flowchart TB
    Start([🌐 Transaction arrives<br/>from network or RPC])
    
    Pool[📦 Added to Transaction Pool<br/>Validated & sorted by priority]
    
    Build([🎯 Beacon chain requests new block<br/>forkchoiceUpdated])
    
    Select[🎰 PayloadBuilder selects<br/>best_transactions by fee]
    
    Execute[⚙️ Execute in EVM<br/>Update balances & storage<br/>Accumulate in TransitionState]
    
    Merge[🔄 merge_transitions<br/>TransitionState → BundleState<br/>Create revert records]
    
    Convert[📝 Convert format<br/>BundleState → PlainPostState<br/>U256 keys → B256 keys]
    
    StateRoot[🌳 Calculate State Root<br/>TrieDB computes Merkle root<br/>342ms on 3G dataset]
    
    Assemble[🏗️ Assemble Block<br/>Put state_root in header]
    
    Complete([✅ Block Ready<br/>Return to beacon chain])
    
    Start --> Pool
    Pool --> Build
    Build --> Select
    Select --> Execute
    Execute --> Merge
    Merge --> Convert
    Convert --> StateRoot
    StateRoot --> Assemble
    Assemble --> Complete
    
    style Start fill:#e3f2fd
    style Pool fill:#fff3e0
    style Build fill:#f3e5f5
    style Execute fill:#ffe0b2
    style Merge fill:#c8e6c9
    style StateRoot fill:#ffcdd2
    style Complete fill:#81c784
```

---

**Now you understand the complete flow from transaction pool to state root!** 🎉
