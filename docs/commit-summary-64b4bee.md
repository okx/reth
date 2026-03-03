# Commit Summary: avoid block re-executing in validator

**Commit**: `64b4bee76f` on branch `benchmark-0302`
**13 files changed**, 616 insertions(+), 19 deletions(-)

## 改动分类

### 1. 跳过 Validator 二次执行（核心优化）

Miner 出块后，validator 阶段（`new_payload`）会重新执行所有交易。通过让 payload builder 保存执行结果，并在 `new_payload` 到达 tree 前通过 `InsertExecutedBlock` 插入，使 tree 走到 `AlreadySeen` 路径跳过执行。

| 文件 | 改动 |
|------|------|
| `crates/ethereum/engine-primitives/src/payload.rs` | `EthBuiltPayload` 加 `executed_block` 字段 + `with_executed_block()` + 覆盖 `executed_block()` trait 方法 |
| `crates/ethereum/payload/src/lib.rs` | `default_ethereum_payload()` 中保留 `BundleState`、`HashedPostState`、`TrieUpdates`，构建 `BuiltPayloadExecutedBlock` 存入 payload |
| `crates/ethereum/payload/Cargo.toml` | 加 `reth-execution-types`、`either` 依赖 |
| `crates/engine/local/src/miner.rs` | `resolve_kind()` 和 `new_payload()` 之间加 `yield_now()` 让主循环先处理 `InsertExecutedBlock` |
| `crates/node/builder/src/launch/engine.rs` | `"inserting built payload"` 日志从 `debug!` 改为 `info!` |
| `crates/rpc/rpc-eth-api/src/helpers/pending_block.rs` | `BlockBuilderOutcome` 解构加 `..` 兼容新字段 |

**验证结果**：`inserting built payload` 100% 出现，`AlreadySeen` 全部命中。

### 2. 出块耗时 Instrumentation

在关键路径上加了计时日志，用于性能分析。

| 文件 | 改动 |
|------|------|
| `crates/engine/local/src/miner.rs` | `advance()` 中加 `cycle_ms`、`idle_ms`、`advance_ms`、`fcu_ms`、`resolve_ms`、`new_payload_ms` 计时 + `last_block_added_at` 字段 |
| `crates/ethereum/payload/src/lib.rs` | `default_ethereum_payload()` 中加 `txpool_next_ms`、`tx_execute_ms`、`finish_ms`、`payload_build_total_ms`、`txs_considered`、`txs_executed`、`changed_accounts`、`changed_storages`、`cumulative_gas_used`、`block_gas_limit` |
| `crates/evm/evm/src/execute.rs` | `finish()` 中加 `state_root_ms`、`assemble_ms` 计时 |
| `crates/evm/evm/Cargo.toml` | 加 `tracing` 依赖 |

### 3. Dev 链 Gas Limit 环境变量支持

| 文件 | 改动 |
|------|------|
| `crates/chainspec/src/spec.rs` | DEV chainspec 支持通过环境变量 `RETH_DEV_GAS_LIMIT`、`RETH_DEV_BASE_FEE_MAX_CHANGE_DENOMINATOR`、`RETH_DEV_BASE_FEE_ELASTICITY_MULTIPLIER` 覆盖 genesis 配置 |

### 4. Osaka RLP Block Size Limit 放大

| 文件 | 改动 |
|------|------|
| `crates/consensus/common/src/validation.rs` | `MAX_RLP_BLOCK_SIZE` 从 8MB (EIP-7934) 改为 16MB，避免高吞吐场景下区块被 RLP 大小限制 |

### 5. 文档和脚本

| 文件 | 说明 |
|------|------|
| `docs/skip-miner-reexecution.md` | 跳过二次执行的设计文档 |
| `docs/state-root-strategies.md` | State root 计算策略分析 |
| `run.sh` | Dev 模式启动脚本 |

## 性能数据（5 万账户，1s 出块）

```
advance_ms (563)
├─ fcu_ms (2)
├─ resolve_ms (501) ≈ payload_build_total_ms (497)
│   ├─ txpool_next_ms (29)
│   ├─ tx_execute_ms (226)
│   ├─ state_root_ms (120)     ← 单线程，下一步优化目标
│   └─ assemble_ms (~79)
└─ new_payload_ms (41)          ← AlreadySeen 已生效，仅 channel round-trip 开销
```

## 发现的其他问题

1. **Osaka RLP 限制导致区块打不满**：EIP-7934 的 8MB 限制在高吞吐场景下比 gas limit 更早触发，通过 `mark_invalid` 移除了大量 sender 的交易。已通过增大 `MAX_RLP_BLOCK_SIZE` 解决。

2. **交易池入池速度是瓶颈**：pool 有 35 万+ pending 交易，但迭代器快照时 `PendingPool.by_id` 只有 4-11 万笔。RPC 层到 pending pool 的处理速度跟不上 benchmark 发送速率。

3. **State root 单线程计算**：payload builder 的 `finish()` 使用 `StateRoot::calculate()` 单线程计算，对 5 万变更账户耗时 120ms。Validator 路径有并行的 `ParallelStateRoot` / `StateRootTask`，但 payload builder 未使用。
