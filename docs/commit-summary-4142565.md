# Commit Summary: support parallel merkle root

**Commit**: `4142565a97` on branch `benchmark-0302`
**10 files changed**, 555 insertions(+), 20 deletions(-)

## 改动概述

为 miner（payload builder）引入可插拔的 state root 计算策略，支持并行计算。

## 架构改动

### 1. StateRootStrategy trait（`crates/evm/evm/src/execute.rs`）

新增 `StateRootStrategy` trait，定义 state root 计算的生命周期：
- `state_hook()` — 执行前获取可选的状态变更 hook（为 Phase 2 StateRootTask 预留）
- `compute_root()` — 执行后计算 state root

两个实现：
- `SyncStateRoot` — 默认同步计算
- `ParallelStrategy`（在 `reth-trie-parallel`）— 并行 storage root 计算

### 2. BlockBuilder trait 扩展（`crates/evm/evm/src/execute.rs`）

- 新增 `finish_with_strategy(state, strategy)` — 接受自定义 state root 策略
- `finish(state)` 改为默认实现，委托给 `finish_with_strategy(state, SyncStateRoot)`
- 所有现有 `finish()` 调用方零改动

### 3. ParallelStrategy（`crates/trie/parallel/src/root.rs`）

- 复用 `ParallelStateRoot` + `OverlayStateProviderFactory`
- 只为有 storage 变更的账户创建并行 task（`std::iter::empty()` 替代 `account_prefix_set`）
- 环境变量 `RETH_PARALLEL_STATE_ROOT=1` 启用，默认走同步

### 4. EthereumPayloadBuilder 集成（`crates/ethereum/payload/src/lib.rs`）

- `default_ethereum_payload()` 新增 `state_root_strategy: S` 泛型参数
- `EthereumPayloadBuilder` 通过 `state_root_strategy()` 方法统一构造策略
- `try_build` 和 `build_empty_payload` 使用同一策略
- `Client` 约束增加 `DatabaseProviderFactory` 相关 bounds
- `EthereumPayloadBuilder` 持有 `Runtime`（从 node context 传入）

### 5. 调用方适配

| 文件 | 改动 |
|------|------|
| `crates/ethereum/node/src/payload.rs` | `EthereumPayloadBuilder::new()` 传入 `runtime` |
| `crates/rpc/rpc-eth-api/src/helpers/pending_block.rs` | 无改动（用 `finish()` 默认同步） |

## 性能优化

### 关键优化：跳过无 storage 变更的并行 task

原始 `ParallelStateRoot` 为所有变更账户创建 storage root task（即使没有 storage 变更）。优化后只为有 storage 变更的账户创建 task：
- ETH 转账（storages=0）：0 个 task，spawn_ms 从 500ms 降到 0ms
- 合约调用（有 storage 变更）：只创建必要的 task

### Benchmark 结果（50k 账户，ETH 转账，1s 出块）

| 版本 | state_root_ms | payload_build_total_ms |
|------|---------------|----------------------|
| 同步 (baseline) | 107.0 | 510.3 |
| **并行 (优化后)** | **100.6** | **490.6** |
| 差异 | **-6%** | **-4%** |

纯 ETH 转账场景收益有限（无 storage 可并行化）。合约调用场景预期收益更大。

## 配置

| 环境变量 | 默认值 | 说明 |
|---------|--------|------|
| `RETH_PARALLEL_STATE_ROOT` | 未设置（同步） | 设为 `1` 或 `true` 启用并行 |

## 文档

| 文件 | 说明 |
|------|------|
| `docs/parallel-state-root-miner-design.md` | 完整设计方案（含 Phase 2 StateRootTask 规划） |
| `docs/benchmark-state-root-comparison.md` | 各版本 benchmark 对比数据 |
| `docs/commit-summary-64b4bee.md` | 上个 commit 的总结 |
