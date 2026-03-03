# State Root 计算策略 Benchmark 对比

**测试条件**：50k 账户，ETH 转账（无 storage 变更），1s 出块，5Ggas limit

## 耗时对比

### 1. 同步版本（SyncStateRoot，baseline）

| 指标 | 平均 (ms) |
|------|-----------|
| tx_execute_ms | 225.8 |
| state_root_ms | 119.6 |
| finish_ms | 198.6 |
| payload_build_total_ms | 497.3 |
| advance_ms | 563.3 |
| idle_ms | 439.9 |

### 2. ParallelStateRoot + spawn_blocking（每账户一个 task）

| 指标 | 平均 (ms) |
|------|-----------|
| tx_execute_ms | - |
| state_root_ms | 500-800 |
| finish_ms | 656.6 |
| payload_build_total_ms | 877.8 |
| advance_ms | 931.5 |
| idle_ms | 227.3 |

**问题**：50k 个 `spawn_blocking` 任务，每个创建 DB provider，开销巨大。

### 3. ParallelStateRoot + rayon（线程池并行，仍创建 50k target）

| 指标 | 平均 (ms) |
|------|-----------|
| tx_execute_ms | 209.3 |
| state_root_ms | 233.2 |
| finish_ms | 313.9 |
| payload_build_total_ms | 587.5 |
| advance_ms | 649.9 |
| idle_ms | 357.0 |

**问题**：rayon 限制线程数，但 50k target 仍需 50k 次 DB provider 创建。

### 4. ParallelStateRoot + rayon + 跳过无 storage 变更的 target（当前）

| 指标 | 平均 (ms) |
|------|-----------|
| tx_execute_ms | 228.0 |
| state_root_ms | **97.4** |
| finish_ms | 187.9 |
| payload_build_total_ms | **490.7** |
| advance_ms | 558.4 |
| idle_ms | 456.0 |

### 5. 同步版本（对照组，同一代码基）

| 指标 | 平均 (ms) |
|------|-----------|
| tx_execute_ms | 232.1 |
| state_root_ms | **107.0** |
| finish_ms | 199.5 |
| payload_build_total_ms | 510.3 |
| advance_ms | 576.1 |
| idle_ms | 430.3 |

## State Root 阶段内部拆分（版本 4）

| 步骤 | 耗时 |
|------|------|
| spawn_ms（parallel storage roots） | 0.004ms（0 个 target） |
| provider_ms（创建 DB provider） | 0.008ms |
| walk_ms（account trie 遍历） | 86-98ms |
| finalize_ms | 0.3ms |
| **total_ms** | **86-98ms** |

## 对比总结

| 版本 | state_root_ms | vs 同步对照 |
|------|---------------|-------------|
| 1. 同步 (早期 baseline) | 119.6 | - |
| 2. spawn_blocking | 500-800 | **+400%** |
| 3. rayon (50k target) | 233.2 | **+95%** |
| 4. **rayon (0 target)** | **97.4** | **-9%** |
| 5. 同步 (对照组) | 107.0 | 基准 |

**结论**：优化后的 ParallelStateRoot（版本 4）比同步版本（版本 5）快约 9%。
在纯 ETH 转账（无 storage 变更）场景下，收益主要来自跳过不必要的 storage root target。
在有 storage 变更的场景（合约调用），rayon 并行化将带来更大收益。
