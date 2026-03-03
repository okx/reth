# Miner State Root 并行化设计方案

## 1. 核心抽象：StateRootStrategy trait

state root 计算应该是一个独立的策略，贯穿整个区块构建生命周期：

```rust
/// 区块构建过程中的 state root 计算策略。
///
/// 生命周期：
/// 1. state_hook() — 执行前，获取可选的状态变更 hook
/// 2. 交易执行 — hook 接收每笔交易的增量状态
/// 3. compute_root() — 执行后，获取最终 state root
pub trait StateRootStrategy: Send {
    /// 返回状态变更 hook，用于在交易执行期间接收增量更新。
    ///
    /// - Synchronous / Parallel: 返回 None（不需要 hook）
    /// - StateRootTask: 返回 hook（连接后台 MultiProofTask）
    fn state_hook(&mut self) -> Option<Box<dyn OnStateHook>>;

    /// 计算 state root。
    ///
    /// - Synchronous: 全量同步计算
    /// - Parallel: 并行 storage root + 顺序 state trie
    /// - StateRootTask: 等待后台任务结果
    fn compute_root(
        self,
        hashed_state: &HashedPostState,
        state_provider: &dyn StateProvider,
    ) -> Result<(B256, TrieUpdates), BlockExecutionError>;
}
```

## 2. 三种策略实现

### Synchronous（默认，零依赖）

```rust
pub struct SyncStateRoot;

impl StateRootStrategy for SyncStateRoot {
    fn state_hook(&mut self) -> Option<Box<dyn OnStateHook>> { None }

    fn compute_root(self, hashed_state: &HashedPostState, sp: &dyn StateProvider)
        -> Result<(B256, TrieUpdates), BlockExecutionError>
    {
        sp.state_root_with_updates(hashed_state.clone())
            .map_err(BlockExecutionError::other)
    }
}
```

### Parallel（Phase 1）

```rust
pub struct ParallelStrategy<F> {
    overlay_factory: OverlayStateProviderFactory<F>,
}

impl<F: ...> StateRootStrategy for ParallelStrategy<F> {
    fn state_hook(&mut self) -> Option<Box<dyn OnStateHook>> { None }

    fn compute_root(self, hashed_state: &HashedPostState, _sp: &dyn StateProvider)
        -> Result<(B256, TrieUpdates), BlockExecutionError>
    {
        let prefix_sets = hashed_state.construct_prefix_sets().freeze();
        let overlay = self.overlay_factory
            .with_extended_hashed_state_overlay(hashed_state.clone_into_sorted());
        ParallelStateRoot::new(overlay, prefix_sets)
            .incremental_root_with_updates()
            .map_err(BlockExecutionError::other)
    }
}
```

### StateRootTask（Phase 2，复用 validator 的 PayloadProcessor）

```rust
pub struct TaskStrategy {
    handle: PayloadHandle<...>,
}

impl StateRootStrategy for TaskStrategy {
    fn state_hook(&mut self) -> Option<Box<dyn OnStateHook>> {
        Some(Box::new(self.handle.state_hook()))
    }

    fn compute_root(mut self, _hashed_state: &HashedPostState, _sp: &dyn StateProvider)
        -> Result<(B256, TrieUpdates), BlockExecutionError>
    {
        let outcome = self.handle.state_root()
            .map_err(BlockExecutionError::other)?;
        Ok((outcome.state_root, outcome.trie_updates))
    }
}
```

## 3. BlockBuilder 接口变更

```rust
pub trait BlockBuilder {
    // ... 现有执行方法不变 ...

    /// 用默认同步策略完成（向后兼容所有现有调用方）
    fn finish(self, state: impl StateProvider) -> Result<BlockBuilderOutcome<...>, ...>
    where Self: Sized
    {
        self.finish_with_strategy(state, SyncStateRoot)
    }

    /// 用指定策略完成区块构建
    fn finish_with_strategy(
        self,
        state: impl StateProvider,
        strategy: impl StateRootStrategy,
    ) -> Result<BlockBuilderOutcome<...>, ...>;
}
```

- `finish()` 保持原签名，作为默认实现调用 `finish_with_strategy(SyncStateRoot)`
- 所有现有调用方零改动
- 需要自定义策略的调用方（miner）使用 `finish_with_strategy()`

## 4. default_ethereum_payload() 重构

```rust
pub fn default_ethereum_payload<EvmConfig, Client, Pool, F>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<...>,
    best_txs: F,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
{
    // --- 1. 准备 ---
    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let mut builder = evm_config.builder_for_next_block(&mut db, ...)?;

    // --- 2. 创建 state root 策略 ---
    let mut strategy = ParallelStrategy::new(
        OverlayStateProviderFactory::new(client.clone(), Default::default())
    );
    let state_hook = strategy.state_hook();
    // Phase 1: hook = None (Parallel 不需要)
    // Phase 2: hook = Some(...) (StateRootTask 需要)

    // --- 3. 执行交易 ---
    for tx in best_txs {
        // Phase 2 时这里会用 hook
        builder.execute_transaction(tx)?;
    }

    // Phase 2: drop hook 通知后台任务执行结束
    drop(state_hook);

    // --- 4. 完成：策略计算 state root + 组装区块 ---
    let outcome = builder.finish_with_strategy(state_provider.as_ref(), strategy)?;

    // --- 5. 构建 payload ---
    Ok(BuildOutcome::Better { payload, cached_reads })
}
```

## 5. Validator 也可以复用（未来）

当前 validator 在 `payload_validator.rs` 中手动做策略选择：

```rust
match strategy {
    StateRootStrategy::StateRootTask => handle.state_root(),
    StateRootStrategy::Parallel => self.compute_state_root_parallel(...),
    StateRootStrategy::Synchronous => {},
}
```

未来可以重构为使用同一个 `StateRootStrategy` trait：

```rust
let strategy = self.plan_state_root_computation();  // 返回 impl StateRootStrategy
let hook = strategy.state_hook();
let output = self.execute_block(state_provider, env, input, hook)?;
let (state_root, trie_updates) = strategy.compute_root(&hashed_state, &state)?;
```

Miner 和 validator 共享同一套策略接口和实现。

## 6. 实施计划

### Phase 1: Parallel

| 步骤 | 改动 |
|------|------|
| 1 | 定义 `StateRootStrategy` trait（放在 `reth-evm` 或新 crate） |
| 2 | 实现 `SyncStateRoot` |
| 3 | 实现 `ParallelStrategy` |
| 4 | `BlockBuilder` trait 新增 `finish_with_strategy()` |
| 5 | `BasicBlockBuilder` 实现 `finish_with_strategy()` |
| 6 | `finish()` 改为默认实现 |
| 7 | `default_ethereum_payload()` 使用 `ParallelStrategy` |

### Phase 2: StateRootTask

| 步骤 | 改动 |
|------|------|
| 1 | 实现 `TaskStrategy`（复用 `PayloadProcessor`） |
| 2 | 交易执行循环支持 state hook |
| 3 | `default_ethereum_payload()` 创建策略时选择 Task |

### 未来：Validator 统一

| 步骤 | 改动 |
|------|------|
| 1 | `payload_validator.rs` 使用 `StateRootStrategy` trait |
| 2 | 删除 validator 中的 ad-hoc 策略选择代码 |

## 7. StateRootStrategy trait 放置位置

| 选项 | 位置 | 优缺点 |
|------|------|--------|
| A | `reth-evm` | 靠近 `BlockBuilder`，但 `reth-evm` 是底层 crate，不应依赖 trie |
| B | `reth-storage-api` | 靠近 `StateRootProvider`，但这是存储层 |
| C | 新 crate `reth-state-root` | 干净的边界，但增加 crate 数量 |
| D | `reth-trie-common` | 已有 trie 相关类型，但可能不适合放策略 trait |

**推荐 A**：trait 本身定义在 `reth-evm`（只依赖 `HashedPostState`、`TrieUpdates`、`StateProvider`、`OnStateHook` 这些已有依赖），具体策略实现放在各自 crate（`SyncStateRoot` 在 `reth-evm`，`ParallelStrategy` 在 `reth-trie-parallel`，`TaskStrategy` 在 `reth-engine-tree`）。
