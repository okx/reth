# Skip Miner Re-execution (Dev Mode 优化)

## 1. 术语说明

| 术语 | 描述 |
|------|------|
| Miner | 使用 `LocalMiner` 本地出块的节点（dev 模式） |
| Validator | Engine Tree 中执行 `new_payload` 时的区块验证路径 |
| AlreadySeen | `insert_block_or_payload` 中检测到 block 已存在 `tree_state` 时跳过执行的快速路径 |
| InsertExecutedBlock | Engine API 内部消息，将已执行的 block 直接插入 `tree_state` |
| BuiltPayloadExecutedBlock | Payload builder 产出的执行结果结构，可转换为 `ExecutedBlock` |

## 2. 背景

### 现状

Reth dev 模式下，`LocalMiner` 既是 miner 又是 validator：

1. **Miner 阶段**：`default_ethereum_payload()` 执行所有交易构建区块
2. **Validator 阶段**：`new_payload` 到达 engine tree 后，重新执行所有交易

每个区块的交易被执行 **两次**，重交易区块多耗 100-300ms。

### 问题根因

两个独立问题导致 `AlreadySeen` 路径从未触发：

1. **`EthBuiltPayload` 不保存执行结果**：`executed_block()` 返回 `None`，`InsertExecutedBlock` 从未发送
2. **即使发送，时序有竞争**：`InsertExecutedBlock` 和 `new_payload` 在 `select!` 中无序竞争

### 2.1 问题

对于重交易区块（如 1.5Ggas 满块），EVM 重执行耗时可达 100-300ms，是出块间隔中最大的可优化项。

### 2.2 需求

Dev 模式下 miner 自产的区块，validator 阶段应跳过二次执行。不要求 100% 命中（退化时全量执行，不影响正确性），要求改动最小化。

## 3. 目标

让 dev 模式下 miner 出块后，`new_payload` 走到 `AlreadySeen` 路径跳过执行。

### 3.1 In Scope

- `EthBuiltPayload` 保存执行结果，使 `executed_block()` 返回 `Some`
- `InsertExecutedBlock` 能正常发送并先于 `new_payload` 到达 tree
- `new_payload` 命中 `AlreadySeen` 跳过 EVM 执行

### 3.2 Out of Scope

- 非 dev 模式的优化
- 100% 时序保证（极小概率退化为全量执行，可接受）
- `EthBuiltPayload::new()` 签名变更（保持向后兼容）

## 4. 方案

### 4.1 整体流程

```
Payload Builder:
  execute txs → finish() → BlockBuilderOutcome (含 bundle_state)
                         → 构建 BuiltPayloadExecutedBlock
                         → 存入 EthBuiltPayload

Engine Launch Loop (select!):
  built_payloads 就绪 → payload.executed_block() → Some(...)
                      → InsertExecutedBlock 发送到 to_tree
                      → tree_state.insert_executed(block)

LocalMiner:
  resolve_kind() 返回 → yield_now() 让主循环先处理 built_payloads
                      → new_payload 发送

Engine Tree:
  new_payload 到达 → sealed_header_by_hash() → Some (block 已在 tree_state)
                   → AlreadySeen ✓ 跳过执行
```

### 4.2 改动清单

#### 改动 1：`BlockBuilderOutcome` 加 `bundle_state`

**文件**：`crates/evm/evm/src/execute.rs`

```rust
pub struct BlockBuilderOutcome<N: NodePrimitives> {
    pub execution_result: BlockExecutionResult<N::Receipt>,
    pub bundle_state: BundleState,       // ★ 新增
    pub hashed_state: HashedPostState,
    pub trie_updates: TrieUpdates,
    pub block: RecoveredBlock<N::Block>,
}
```

`finish()` 方法在返回前用 `std::mem::take(&mut db.bundle_state)` 取出 bundle state。现有调用方使用 `..` 解构不受影响。

#### 改动 2：`EthBuiltPayload` 加可选字段 + 覆盖 trait

**文件**：`crates/ethereum/engine-primitives/src/payload.rs`

结构体加字段（默认 `None`，`new()` 签名不变）：

```rust
pub(crate) executed_block: Option<BuiltPayloadExecutedBlock<N>>,
```

加 builder 方法：

```rust
pub fn with_executed_block(mut self, block: BuiltPayloadExecutedBlock<N>) -> Self {
    self.executed_block = Some(block);
    self
}
```

覆盖 `BuiltPayload` trait 的默认实现：

```rust
fn executed_block(&self) -> Option<BuiltPayloadExecutedBlock<Self::Primitives>> {
    self.executed_block.clone()
}
```

#### 改动 3：`default_ethereum_payload()` 构建执行结果

**文件**：`crates/ethereum/payload/src/lib.rs`

完整解构 `BlockBuilderOutcome`，构建 `BuiltPayloadExecutedBlock`：

```rust
let BlockBuilderOutcome { execution_result, bundle_state, hashed_state, trie_updates, block } =
    builder.finish(state_provider.as_ref())?;

let requests = chain_spec
    .is_prague_active_at_timestamp(attributes.timestamp)
    .then(|| execution_result.requests.clone());

let executed = BuiltPayloadExecutedBlock {
    recovered_block: Arc::new(block),
    execution_output: Arc::new(BlockExecutionOutput {
        result: execution_result,
        state: bundle_state,
    }),
    hashed_state: Either::Left(Arc::new(hashed_state)),
    trie_updates: Either::Left(Arc::new(trie_updates)),
};

let payload = EthBuiltPayload::new(attributes.id, sealed_block, total_fees, requests)
    .with_sidecars(blob_sidecars)
    .with_executed_block(executed);
```

#### 改动 4：Miner 加 `yield_now()`

**文件**：`crates/engine/local/src/miner.rs`

`resolve_kind()` 之后、`new_payload()` 之前加一行：

```rust
tokio::task::yield_now().await;
```

### 4.3 时序保证分析

1. `resolve_kind()` 返回时，payload service 已通过 `broadcast::send()` 同步发送到 `built_payloads` 流
2. `yield_now()` 让 miner 让出执行权，**此时 `new_payload` 尚未发送**
3. 主循环获得执行权 → `built_payloads` 就绪 → `InsertExecutedBlock` 送入 `to_tree`
4. Miner 恢复 → `new_payload` 经 `incoming_requests` → engine handler → `to_tree`
5. `to_tree` 是 FIFO crossbeam channel，`InsertExecutedBlock` 必然先被消费

**退化场景**：极小概率下主循环未及时处理 → `new_payload` 先到达 tree → 全量执行。正确性不受影响。

## 5. 性能指标

| 指标 | 优化前 | 优化后 |
|------|--------|--------|
| new_payload 延迟（重交易块） | 100-300ms | < 1ms（AlreadySeen） |
| 每区块 EVM 执行次数 | 2 次 | 1 次 |
| 退化概率 | N/A | 极低（退化 = 回到优化前） |

## 6. 改动总量

| 文件 | 新增行数 | 类型 |
|------|---------|------|
| `crates/evm/evm/src/execute.rs` | +2 | 结构体 + finish() |
| `crates/ethereum/engine-primitives/src/payload.rs` | +15 | 字段 + trait 覆盖 |
| `crates/ethereum/payload/src/lib.rs` | +10 | 构建执行结果 |
| `crates/engine/local/src/miner.rs` | +1 | yield_now |
| **总计** | **~28** | **4 个文件，0 个新类型/文件** |
