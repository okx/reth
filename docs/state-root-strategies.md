# Reth State Root 计算策略总结

## 概述

Reth 目前支持 **三种** state root 计算策略，分别适用于不同场景。Validator（引擎树路径）可以使用全部三种策略并自动降级，而 Miner/Payload Builder 目前只支持同步方式。

---

## 三种计算策略

### 1. StateRootTask（稀疏 Trie，最优）

- **位置**：`crates/engine/tree/src/tree/payload_processor/sparse_trie.rs`
- **核心类型**：`SparseTrieCacheTask<A, S>`、`StateRootComputeOutcome`
- **原理**：
  - 基于稀疏 Trie（Sparse Trie），节点懒加载，只在需要时通过 multiproof 拉取
  - 后台任务并行生成 multiproof，与 EVM 执行流水线重叠
  - `ParallelSparseTrie`：将 trie 拆分为 upper trie（深度 <2）+ 256 个 lower subtrie，lower subtrie 并行更新哈希
  - 跨区块复用稀疏 trie 缓存（`preserved_sparse_trie`），减少重复加载
  - 包含 `PrewarmCacheTask` 提前预热节点
- **特点**：非阻塞（后台任务），速度最快，内存占用最大
- **启用条件**：`has_enough_parallelism`（CPU ≥5 核）且 `!legacy_state_root`

### 2. ParallelStateRoot（并行，中等）

- **位置**：`crates/trie/parallel/src/root.rs`
- **核心类型**：`ParallelStateRoot<Factory>`
- **原理**：
  - 并行计算每个账户的 storage root（rayon）
  - 所有 storage root 完成后，顺序走 state trie 得到最终根
- **特点**：阻塞调用者，速度中等，内存占用中等
- **接口**：
  ```rust
  pub fn incremental_root(self) -> Result<B256, ParallelStateRootError>
  pub fn incremental_root_with_updates(self) -> Result<(B256, TrieUpdates), ParallelStateRootError>
  ```

### 3. StateRoot（同步顺序，兜底）

- **位置**：`crates/trie/trie/src/trie.rs`
- **核心类型**：`StateRoot<T, H>`
- **原理**：
  - 单线程顺序遍历整棵 trie
  - 使用数据库游标（trie cursor + hashed cursor）
- **特点**：阻塞，速度最慢，内存占用最小，任何情况下都可用
- **接口**：
  ```rust
  pub fn root(self) -> Result<B256, StateRootError>
  pub fn root_with_updates(self) -> Result<(B256, TrieUpdates), StateRootError>
  ```

---

## Validator vs Miner 支持对比

### Validator（引擎树路径）

**策略选择逻辑**（`crates/engine/tree/src/tree/payload_validator.rs`）：

```rust
fn plan_state_root_computation(&self) -> StateRootStrategy {
    if self.config.state_root_fallback() {
        StateRootStrategy::Synchronous          // 强制同步（测试用）
    } else if self.config.use_state_root_task() {
        StateRootStrategy::StateRootTask        // 首选：稀疏 Trie 后台任务
    } else {
        StateRootStrategy::Parallel             // 次选：并行计算
    }
}
```

**降级链**：
```
StateRootTask
    ↓ (超时，默认 1 秒)
ParallelStateRoot
    ↓ (出错)
StateRoot（同步）
```

**支持的三种策略**：

| 策略 | 条件 | 特点 |
|------|------|------|
| StateRootTask | ≥5 核 CPU 且未设 `legacy_state_root` | 后台并行，最快 |
| Parallel | StateRootTask 超时或不可用 | 并行 storage root，中等速度 |
| Synchronous | `state_root_fallback=true` 或兜底 | 单线程，最慢但最稳 |

### Miner / Payload Builder

**当前实现**（`crates/evm/evm/src/execute.rs`，约 489 行）：

```rust
fn finish(self, state: impl StateProvider) -> Result<BlockBuilderOutcome<N>, BlockExecutionError> {
    let hashed_state = state.hashed_post_state(&db.bundle_state);
    let (state_root, trie_updates) = state
        .state_root_with_updates(hashed_state.clone())   // 仅同步方式
        .map_err(BlockExecutionError::other)?;
    // ...
}
```

**只支持一种策略**：

| 策略 | 状态 |
|------|------|
| StateRootTask | ❌ 不支持 |
| Parallel | ❌ 不支持 |
| Synchronous | ✅ 唯一选项 |

- 在 `crates/ethereum/payload/src/lib.rs` 中调用 `builder.finish(state_provider)`
- 无并行化，无后台任务，无降级机制
- 对大区块有明显性能影响

---

## 综合对比

| 维度 | StateRootTask | Parallel | Synchronous |
|------|:---:|:---:|:---:|
| **代码位置** | `engine/tree/payload_processor/` | `trie/parallel/` | `trie/trie/` |
| **Validator 使用** | ✅ 首选 | ✅ 备选 | ✅ 兜底 |
| **Miner 使用** | ❌ | ❌ | ✅ 唯一 |
| **是否阻塞** | 否（后台任务） | 是 | 是 |
| **并行度** | 高（sparse subtrie + multiproof） | 中（仅 storage root） | 无 |
| **速度** | 最快 | 中等 | 最慢 |
| **内存** | 高（trie 缓存） | 中 | 低 |
| **跨块缓存** | ✅ | ❌ | ❌ |

---

## 配置选项（Validator）

位于 `crates/engine/primitives/src/config.rs`（`TreeConfig`）：

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `legacy_state_root` | bool | false | 禁用 StateRootTask，退回 Parallel |
| `state_root_fallback` | bool | false | 强制使用同步方式（测试用） |
| `has_enough_parallelism` | bool | 自动检测 | 系统 CPU ≥5 核 |
| `state_root_task_timeout` | Option<Duration> | 1 秒 | StateRootTask 超时后降级 |
| `always_compare_trie_updates` | bool | false | 对比不同策略结果（调试） |
| `sparse_trie_prune_depth` | usize | 4 | Trie 剪枝深度（控制内存） |
| `sparse_trie_max_storage_tries` | usize | 100 | 最大缓存 storage trie 数 |

---

## Stages 同步管线（非引擎路径）

**Merkle Stage**（`crates/stages/stages/src/stages/merkle.rs`）：
- 仅使用同步 `StateRoot`
- **增量模式**：逐批处理区块（阈值 7,000 块）
- **重建模式**：变化量超过 100,000 块时全量重建

---

## 架构关系图

```
                   ┌─────────────────────────────────┐
                   │           Validator              │
                   │        (Engine Tree)             │
                   └──────────────┬──────────────────┘
                                  │ plan_state_root_computation()
                    ┌─────────────┼─────────────┐
                    ▼             ▼             ▼
          StateRootTask     Parallel       Synchronous
         (sparse trie,    (parallel      (sequential
          background)      storage        trie walk)
                           roots)
         ──超时(1s)──►      ──出错──►

   ┌─────────────────────────────────┐
   │        Miner / Payload Builder  │
   │         (BlockBuilder::finish)  │
   └──────────────┬──────────────────┘
                  │
                  ▼
             Synchronous（唯一选项）

   ┌─────────────────────────────────┐
   │        Stages Pipeline          │
   │          (Merkle Stage)         │
   └──────────────┬──────────────────┘
                  │
                  ▼
             Synchronous（唯一选项）
```
