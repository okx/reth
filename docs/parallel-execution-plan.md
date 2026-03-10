# True Parallel Execution Migration Plan: fafo → reth

## Overview

将 fafo 的真正并行执行引擎迁移到 reth 中，目标是达到 ~1M TPS。不是引用 fafo 作为库，而是在 reth 中实现一套功能一致的并行执行系统。

## Architecture Comparison

### fafo 执行流水线
```
Mempool → Simulator(4 shards) → Framer(ParaBloom) → Dispatcher(warmup+execute) → StateCache → ADS
                                                         ↓
                                               warmup_tpool (12 threads)
                                               exe_tpool (64 threads rayon)
```

### reth 当前执行流水线
```
TxPool → PayloadBuilder → BlockBuilder.execute_transaction() [serial] → State<DB> → BundleState
```

### 目标架构
```
TxPool → PayloadBuilder → Simulator → Framer → Dispatcher → ParallelBlockExecutor → BundleState
                                                    ↓
                                          warmup_tpool (12 threads)
                                          exe_tpool (64 threads rayon)
```

## Key Concepts Mapping: fafo → reth

| fafo | reth equivalent (to build) | Description |
|------|---------------------------|-------------|
| `BlockContext` | `ParallelBlockContext` | 持有 StateCache、TasksManager、Dashboard，管理整个区块的并行执行 |
| `TxContext` (DatabaseRef) | `ParallelTxDatabase` | 三层读取: StateCache → StateProvider → QMDB/MDBX |
| `StateCache` | `ParallelStateCache` (已有基础) | DashMap 并发缓存，支持 curr_state/prev_state |
| `Dispatcher` | `Dispatcher` | 调度器：warmup + batch coordination + execute |
| `Dashboard` | `Dashboard` | 无锁依赖图：LinkedList + BitVec + AtomicI32 |
| `DTask` | `DispatchTask` | 包装 BlockContext + task index，实现 Dispatchable |
| `ExeTask` | `ExeTask` (已有) | 交易组 + AccessSet + ChangeSets |
| `Framer` | `Framer` (已有基础) | ParaBloom 冲突检测 + 分帧 |
| `Simulator` | `Simulator` (已有基础) | 预模拟提取 CrwSets |
| `ExePipe` | `ParallelExecutionPipeline` | 编排器：串联 Simulator → Framer → Dispatcher |
| `execute_tx()` | `parallel_execute_tx()` | 使用 revm 执行单笔交易，收集 state diff |
| `ChangeSet` | revm `HashMap<Address, Account>` | 状态变更集（复用 revm 原生格式） |
| `AccessSet` | `AccessSet` (对应已有 CrwSets) | 读写集（rdo_set + rnw_set） |

---

## Current Status (已完成的部分)

以下模块在 `crates/xlayer/parallel-exec/` 中已有基础实现（55 tests passing）：
- ✅ **CrwSets** (`crw_sets.rs`): 短哈希 + extract_crw_sets
- ✅ **ParaBloom** (`para_bloom.rs`): 并行 Bloom filter 冲突检测
- ✅ **Simulator** (`simulator.rs`): 2 线程专用 rayon pool + CrwSets 提取
- ✅ **ExeTask** (`task.rs`): SimResult 结构
- ✅ **Framer** (`framer.rs`): 基础分帧逻辑
- ✅ **ParallelStateCache** (`cache.rs`): DashMap 并发缓存（基础版）

**需要新建的核心模块：**
- 🆕 Dashboard (无锁依赖图)
- 🆕 Dispatcher (并行调度引擎)
- 🆕 ParallelTxDatabase (三层 DatabaseRef)
- 🆕 ParallelBlockContext (区块执行上下文)
- 🆕 parallel_execute_tx (revm 并行执行)
- 🆕 TasksManager
- 🆕 ParallelExecutionPipeline (编排器)

---

## Task Breakdown

### Phase 0: 准备工作

#### Task 0.1: 清理当前 preWarm 代码
- **文件**: `crates/ethereum/payload/src/lib.rs`, `crates/ethereum/payload/src/parallel.rs`
- **操作**:
  - 移除 parallel path 中的 background simulation 分支（当前只做 preWarm，浪费 CPU）
  - 删除 `SimDatabaseRef`（将被 `ParallelTxDatabase` 替代）
  - 保留 `EthereumBuilderConfig.parallel_exec` flag 和 `--xlayer.parallel-exec` CLI 参数
  - 恢复 parallel path 为纯串行执行（和 non-parallel path 一致），等 Phase 8 再接入真正并行
- **原因**: 当前 preWarm 实现不提供真正并行执行，反而降低性能 30%

#### Task 0.2: 整理 parallel-exec crate 模块结构
- **文件**: `crates/xlayer/parallel-exec/src/lib.rs`
- **操作**: 规划新模块目录结构，确保现有模块可被复用

---

### Phase 1: ParallelStateCache 升级 + ParallelTxDatabase

#### Task 1.1: 升级 ParallelStateCache 支持 curr/prev 双层
- **文件**: `crates/xlayer/parallel-exec/src/cache.rs`
- **参考**: fafo `exepipe-common/src/statecache.rs`
- **操作**:
  - 增加 `apply_state_diff()` 方法：将 revm 执行后的 `HashMap<Address, Account>` state diff 应用到缓存
  - 增加 `lookup_account()` / `lookup_storage()` 方法：从缓存中查找
  - 支持 curr_state → prev_state 两层查找
- **fafo 对应**: `StateCache.lookup_value()` + `apply_change()`

#### Task 1.2: 实现 ParallelTxDatabase
- **文件**: 新建 `crates/xlayer/parallel-exec/src/tx_database.rs`
- **参考**: fafo `exepipe/src/context/tx_context.rs`
- **操作**:
  ```rust
  pub struct ParallelTxDatabase<'a> {
      curr_state: &'a ParallelStateCache,
      prev_state: Option<&'a ParallelStateCache>,
      provider: &'a (dyn StateProvider + Sync),
  }

  impl revm::DatabaseRef for ParallelTxDatabase<'_> {
      fn basic_ref(&self, address: Address) -> ... {
          // 1. Check curr_state
          // 2. Check prev_state
          // 3. Fall through to StateProvider
      }
      fn storage_ref(&self, address: Address, index: U256) -> ... { /* same */ }
      fn code_by_hash_ref(&self, code_hash: B256) -> ... { /* same */ }
      fn block_hash_ref(&self, number: u64) -> ... { /* provider */ }
  }
  ```
- **关键**: 实现 `revm::DatabaseRef` 让 revm 可以从并发缓存读取

#### Task 1.3: 单元测试
- ParallelStateCache apply + lookup 正确性
- ParallelTxDatabase 三层 fallback 正确性
- 并发读写安全性测试

---

### Phase 2: Dashboard（无锁依赖图）

#### Task 2.1: 实现 Dashboard 核心数据结构
- **文件**: 新建 `crates/xlayer/parallel-exec/src/dashboard.rs`
- **参考**: fafo `exepipe/src/dispatcher/dashboard.rs`
- **数据结构**:
  ```rust
  pub struct Dashboard {
      all_done_index: AtomicI32,          // 最高连续完成的 task index
      valid_count: AtomicI32,             // 本区块 task 总数
      warmed_counts: Vec<AtomicI32>,      // 每 batch 已 warmup 数
      executed_bitvec: Vec<AtomicU64>,    // task 完成 bitvec
      ignite_ll: Vec<LinkedListItem>,     // 依赖链表
      batch_size: i32,                    // BATCH_SIZE = 4096
  }

  struct LinkedListItem {
      to_ignite: AtomicI32,  // 完成后需点燃的 task head
      next: AtomicI32,       // 链表 next
  }
  ```
- **核心方法**:
  - `set_eei(my_idx, eei)`: 将 my_idx 插入 eei 的点燃链表（原子 swap）
  - `set_executed(idx)`: 标记完成 + 更新 all_done_index
  - `get_ignited_list(idx) -> Vec<i32>`: 遍历链表获取被点燃的 tasks
  - `send_batch_end(idx) -> Option<(batch_end, is_last)>`: batch 协调

#### Task 2.2: Dashboard 并发测试
- 多线程并发 set_eei + set_executed 不丢失
- ignite_ll 链表遍历正确性
- all_done_index 单调递增

---

### Phase 3: Dispatcher（并行调度引擎）— 核心难点

#### Task 3.1: 定义 Dispatchable trait
- **文件**: 新建 `crates/xlayer/parallel-exec/src/dispatcher/mod.rs`
- **参考**: fafo `exepipe/src/dispatcher/dtask.rs`
- **操作**:
  ```rust
  pub trait Dispatchable: Send + Sync + Clone {
      fn warm_up(&self) -> i32;           // 返回 EEI
      fn execute(&self);                  // 真正执行
      fn get_dashboard(&self) -> &Dashboard;
      fn get_idx(&self) -> i32;
      fn get_sibling(&self, idx: i32) -> Self;
      fn end_block(&self);
  }
  ```

#### Task 3.2: 实现 Dispatcher 核心
- **文件**: 新建 `crates/xlayer/parallel-exec/src/dispatcher/dispatcher.rs`
- **参考**: fafo `exepipe/src/dispatcher/dispatcher.rs`
- **数据结构**:
  ```rust
  pub struct Dispatcher<D: Dispatchable> {
      warmup_tpool: threadpool::ThreadPool,    // 12 threads
      exe_tpool: rayon::ThreadPool,            // 64 threads
      batch_senders: Vec<Sender<(D, i32, bool)>>,   // SCAN_COUNT=4
      batch_receivers: Vec<Receiver<(D, i32, bool)>>,
      finished_height: AtomicI64,
  }
  ```
- **核心方法**:
  - `add(task)`: 提交到 warmup_tpool → warm_up() → handle_warmed()
  - `handle_warmed(task, eei)`: 设置 Dashboard EEI，协调 batch 完成
  - `run_execute_thread(thread_id)`: 长运行线程，轮询 batch channel
  - `run_batch(handler, batch_end)`: batch 内循环执行

#### Task 3.3: 实现 EEI 计算
- **参考**: fafo `DTask::warm_up()` 的反向扫描逻辑
- **算法**:
  1. 获取当前 task 的 `task_out_start`（Frame flush 时设置）
  2. 从 `task_out_start` 向前扫描，找最后一个有冲突的 task
  3. 扫描窗口: `EARLY_EXE_WINDOW_SIZE=128`
  4. 用 `ExeTask::has_collision()` 检查 AccessSet 冲突
  5. 返回 EEI 给 Dashboard

#### Task 3.4: 实现 Batch 协调
- **参考**: fafo `Dispatcher::run_batch()` + `Dashboard::send_batch_end()`
- **操作**:
  - SCAN_COUNT=4 个专用线程轮询 batch channel
  - `while all_done_index < batch_end`:
    - 检查连续完成的 tasks → 推进 all_done_index
    - 获取 ignited_list → 派发到 exe_tpool
  - 用 `fetch_max` 原子操作协调多个 scan 线程

#### Task 3.5: Dispatcher 测试
- 简单 task 执行顺序正确
- 有依赖的 task 按正确顺序执行
- batch 边界处理正确
- 压力测试: 大量 task 并发调度

---

### Phase 4: ParallelBlockContext + 交易执行

#### Task 4.1: 实现 TasksManager
- **文件**: 新建 `crates/xlayer/parallel-exec/src/tasks_manager.rs`
- **参考**: fafo `exepipe/src/tasks_manager.rs`
- **操作**:
  ```rust
  pub struct TasksManager {
      tasks: Vec<RwLock<Option<ExeTask>>>,
  }
  ```
- 提供 `task_for_read()` / `task_for_write()` / `set_task()` 方法

#### Task 4.2: 实现 ParallelBlockContext
- **文件**: 新建 `crates/xlayer/parallel-exec/src/block_context.rs`
- **参考**: fafo `exepipe/src/context/block_context.rs`
- **数据结构**:
  ```rust
  pub struct ParallelBlockContext {
      pub tasks_manager: Arc<TasksManager>,
      pub curr_state: Arc<ParallelStateCache>,
      pub prev_state: Arc<ParallelStateCache>,
      pub dashboard: Arc<Dashboard>,
      pub block_env: BlockEnv,
      pub cfg_env: CfgEnv,
      pub state_provider: Arc<dyn StateProvider + Sync>,
      results: Vec<RwLock<Option<TaskExecutionResult>>>,
      gas_used: AtomicU64,
  }
  ```
- **核心方法**: `execute_task(idx)`:
  1. 从 TasksManager 取出 ExeTask
  2. 为每笔 tx 创建 `ParallelTxDatabase` (reads from curr_state → prev_state → provider)
  3. 用 `CacheDB::new(&tx_db)` 包装，创建 revm EVM 执行
  4. 执行交易，收集 `ResultAndState`
  5. 将 state diff 立即应用到 `curr_state`（让后续 task 能看到）
  6. 存储执行结果

#### Task 4.3: 实现 parallel_execute_tx
- **文件**: 新建 `crates/xlayer/parallel-exec/src/execute.rs`
- **参考**: fafo `exepipe-common/src/executor/execute_tx.rs`
- **操作**:
  ```rust
  pub fn parallel_execute_tx<DB: revm::DatabaseRef>(
      db: DB,
      block_env: &BlockEnv,
      cfg_env: &CfgEnv,
      tx_env: TxEnv,
  ) -> Result<ResultAndState, EVMError> {
      let cache_db = CacheDB::new(db);
      let ctx = Context::mainnet()
          .with_db(cache_db)
          .with_cfg(cfg_env.clone())
          .with_block(block_env.clone())
          .build_mainnet();
      let mut evm = EthEvm::new(ctx, false);
      evm.transact(tx_env)
  }
  ```
- **关键差异 vs fafo**: 直接使用 revm 原生 `ResultAndState`，不转换为自定义 ChangeSet

#### Task 4.4: state diff → ParallelStateCache 更新
- **文件**: `crates/xlayer/parallel-exec/src/cache.rs` (扩展)
- **操作**: 将 `ResultAndState.state` (`HashMap<Address, Account>`) 转换为 ParallelStateCache 的更新
  - Account info (nonce, balance, code_hash)
  - Storage slots
  - Contract code
- **fafo 对应**: `curr_state.apply_change(&change_set)`

---

### Phase 5: DispatchTask（连接 Dispatcher ↔ BlockContext）

#### Task 5.1: 实现 DispatchTask
- **文件**: 新建 `crates/xlayer/parallel-exec/src/dispatcher/dispatch_task.rs`
- **参考**: fafo `exepipe/src/dispatcher/dtask.rs`
- **操作**:
  ```rust
  pub struct DispatchTask {
      pub blk_ctx: Arc<ParallelBlockContext>,
      pub idx: i32,
  }

  impl Dispatchable for DispatchTask {
      fn warm_up(&self) -> i32 {
          // 1. 获取 task 的 task_out_start
          // 2. 反向扫描冲突 → 返回 EEI
      }
      fn execute(&self) {
          self.blk_ctx.execute_task(self.idx as usize);
          self.blk_ctx.dashboard.set_executed(self.idx);
      }
      fn get_dashboard(&self) -> &Dashboard { &self.blk_ctx.dashboard }
      fn get_idx(&self) -> i32 { self.idx }
      fn get_sibling(&self, idx: i32) -> Self {
          DispatchTask { blk_ctx: self.blk_ctx.clone(), idx }
      }
  }
  ```

---

### Phase 6: Framer 升级

#### Task 6.1: 升级 Framer 直接输出到 Dispatcher
- **文件**: `crates/xlayer/parallel-exec/src/framer.rs`
- **参考**: fafo `exepipe/src/framer.rs`
- **操作**:
  - 增加 channel 输入: `task_sender: Sender<(usize, Option<ExeTask>)>`
  - `flush_frame()` 时:
    - 设置每个 task 的 `task_out_start`
    - 写入 TasksManager
    - 调用 `dispatcher.add(DispatchTask { blk_ctx, idx })`
  - 增加 `run()` 方法: 循环从 channel 接收 task，调用 `add_task()`

#### Task 6.2: 升级 ExeTask
- **文件**: `crates/xlayer/parallel-exec/src/task.rs`
- **操作**:
  - 增加 `task_out_start: AtomicU32`（Frame flush 时设置，用于 EEI 计算）
  - 增加 `change_sets: Option<Arc<Vec<...>>>` 存储执行后的 state diffs
  - 增加 `tx_envs: Vec<TxEnv>` 用于真正执行时的交易环境
  - 保留 `access_set` 用于 `has_collision()` 冲突检测

---

### Phase 7: ParallelExecutionPipeline（编排器）

#### Task 7.1: 实现 Pipeline
- **文件**: 新建 `crates/xlayer/parallel-exec/src/pipeline.rs`
- **参考**: fafo `exepipe/src/lib.rs` 的 `run_block()` / `run_block_with_sim()`
- **操作**:
  ```rust
  pub struct ParallelExecutionPipeline {
      simulator: Simulator,
      dispatcher: Arc<Dispatcher<DispatchTask>>,
      prev_state: Arc<ParallelStateCache>,
  }

  impl ParallelExecutionPipeline {
      pub fn execute_block(
          &mut self,
          txs: Vec<(TxEnv, Address)>,  // 交易环境 + sender
          block_env: &BlockEnv,
          cfg_env: &CfgEnv,
          state_provider: Arc<dyn StateProvider + Sync>,
      ) -> ParallelBlockResult {
          // 1. 创建 ParallelBlockContext (with curr_state, prev_state)
          // 2. Simulator: 提取 CrwSets
          // 3. Framer (channel-based): 分帧 → 送入 Dispatcher
          // 4. Dispatcher: warmup + parallel execute
          // 5. 收集结果 (按 original_index 排序)
          // 6. self.prev_state = curr_state (为下个区块保留)
      }
  }
  ```

#### Task 7.2: 定义 ParallelBlockResult
- **操作**:
  ```rust
  pub struct ParallelBlockResult {
      pub tx_results: Vec<ParallelTxResult>,  // 按原始顺序
      pub gas_used: u64,
  }

  pub struct ParallelTxResult {
      pub original_index: usize,
      pub execution_result: ExecutionResult,
      pub state_diff: HashMap<Address, Account>,  // revm state diff
      pub gas_used: u64,
  }
  ```

---

### Phase 8: Payload Builder 集成

#### Task 8.1: 在 Ethereum payload builder 中集成
- **文件**: `crates/ethereum/payload/src/lib.rs`
- **操作**: 当 `parallel_exec=true` 时:
  1. 从 tx pool 收集所有候选交易 → `Vec<(Recovered<TransactionSigned>, TxEnv)>`
  2. 调用 `ParallelExecutionPipeline::execute_block()`
  3. 从 `ParallelBlockResult` 构建:
     - `Vec<Receipt>` (按序，cumulative_gas_used 递增)
     - `BundleState` (合并所有 task 的 state diffs)
     - `BlockExecutionResult`
  4. 计算 state root（QMDB 模式可跳过: `skip_state_root_validation=true`）
  5. 构建 `BuiltPayloadExecutedBlock` + `EthBuiltPayload`

#### Task 8.2: BundleState 构建
- **操作**:
  - 按 task 执行顺序合并 state diffs 为 `BundleState`
  - 处理 account create/modify/delete
  - 处理 storage slot changes
  - 处理 contract deployments
  - 确保 `BundleState.reverts` 正确（用于 reorg）

#### Task 8.3: Receipt 构建
- **操作**:
  - 按原始交易顺序排列
  - 累计 gas: `receipt[i].cumulative_gas_used = sum(gas[0..=i])`
  - 组装 logs + bloom filter
  - 确保和串行路径产生完全一致的结果

#### Task 8.4: Fallback 机制
- 并行执行失败/异常时自动退回串行执行
- 日志记录切换原因

---

### Phase 9: 测试和验证

#### Task 9.1: 单元测试
- Dashboard 原子操作正确性
- Dispatcher EEI 计算正确性
- ParallelTxDatabase 三层读取正确性
- state diff → ParallelStateCache 更新正确性

#### Task 9.2: 集成测试（最重要）
- **并行 vs 串行结果一致性**: 给定相同交易序列，并行和串行执行产生完全相同的:
  - receipts (包括 cumulative_gas_used, logs, status)
  - state root
  - BundleState
- 简单转账（无冲突）并行执行
- 有冲突交易的依赖链正确执行
- 多区块连续执行（prev_state 传递）

#### Task 9.3: 性能基准测试
- 串行 vs 并行 TPS 对比
- 不同线程数的性能曲线
- 不同冲突率的性能表现
- 内存使用分析

---

## Implementation Priority & Timeline

```
Phase 0 (准备)           → 0.5 天   清理 preWarm，整理模块
Phase 1 (StateCache+DB)  → 1.5 天   ParallelStateCache 升级 + ParallelTxDatabase
Phase 2 (Dashboard)      → 1.5 天   无锁依赖图
Phase 3 (Dispatcher)     → 3 天     ← 核心难点，从 fafo 精确移植
Phase 4 (BlockContext)    → 2 天     ← 核心难点，revm 集成
Phase 5 (DispatchTask)   → 0.5 天   连接层
Phase 6 (Framer 升级)    → 1 天     channel + Dispatcher 对接
Phase 7 (Pipeline)       → 1 天     编排器
Phase 8 (Payload 集成)   → 2 天     BundleState/Receipt 构建
Phase 9 (测试)           → 2 天     一致性验证 + benchmark
                         --------
                         ~15 天
```

**建议实施顺序**: 0 → 1 → 2 → 3 → 5 → 4 → 6 → 7 → 8 → 9

（Phase 2 和 Phase 3 是最核心的部分，建议优先攻克）

---

## Critical Design Decisions

### 1. State Diff 格式
- **fafo**: 自定义 `ChangeSet` + `apply_op_in_range()`
- **reth**: 使用 revm 原生 `ResultAndState.state: HashMap<Address, Account>`
- **理由**: 复用 revm 基础设施，减少转换开销

### 2. revm State 管理
- **方案**: 每个 worker 使用独立的 `CacheDB<&ParallelTxDatabase>`
- CacheDB 是轻量的，包装 DatabaseRef 并缓存本次交易的读取
- 执行后 state diff 立即 apply 到 ParallelStateCache（curr_state）

### 3. 跨区块缓存
- **串行路径**: 继续使用 reth 原生 `CachedReads`
- **并行路径**: `prev_state: Arc<ParallelStateCache>` 承担跨区块缓存功能
- 两套独立，互不干扰

### 4. State Root
- QMDB 模式: `skip_state_root_validation=true`，跳过（已配置）
- 标准模式: 收集完所有 state diff 后串行计算

### 5. Receipt 构建
- **fafo**: 不构建标准 Receipt（benchmark 工具）
- **reth**: 必须构建完整 Receipt（cumulative_gas_used, logs, bloom, status）
- **方案**: 从每个 task 的 `ExecutionResult` 中提取，按原始 index 排序后组装

---

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Dashboard 无锁逻辑 bug | High | 从 fafo 精确移植 + 大量并发测试 |
| revm 并发安全 | High | 每个 worker 独立 CacheDB，只共享 read-only 的 ParallelTxDatabase |
| BundleState 合并正确性 | High | 严格按 task 顺序合并，串行/并行结果一致性测试 |
| Receipt 顺序错误 | Medium | original_index 追踪 + 排序 |
| 性能不达预期 | Medium | 分阶段 benchmark，每个 Phase 验证 |
| curr_state 写入竞争 | Medium | DashMap 内部分片，粒度是 key 级别，天然低争用 |

---

## Target File Structure

```
crates/xlayer/parallel-exec/src/
├── lib.rs                    # 模块导出
├── crw_sets.rs               # ✅ 已有 - 短哈希 + CrwSets 提取
├── para_bloom.rs             # ✅ 已有 - 并行 Bloom filter
├── simulator.rs              # ✅ 已有 - 预模拟（需小升级）
├── task.rs                   # ✅ 已有 - ExeTask（需升级）
├── framer.rs                 # ✅ 已有 - 分帧器（需升级输出到 Dispatcher）
├── cache.rs                  # ✅ 已有 - ParallelStateCache（需升级 curr/prev）
├── tx_database.rs            # 🆕 ParallelTxDatabase (DatabaseRef 三层读取)
├── execute.rs                # 🆕 parallel_execute_tx (revm 并行执行)
├── tasks_manager.rs          # 🆕 TasksManager (RwLock<Vec<Option<ExeTask>>>)
├── block_context.rs          # 🆕 ParallelBlockContext (区块执行上下文)
├── dashboard.rs              # 🆕 Dashboard (无锁依赖图)
├── pipeline.rs               # 🆕 ParallelExecutionPipeline (编排器)
├── result.rs                 # 🆕 ParallelBlockResult / ParallelTxResult
└── dispatcher/
    ├── mod.rs                # 🆕 Dispatchable trait + re-exports
    ├── dispatcher.rs         # 🆕 Dispatcher 核心调度
    └── dispatch_task.rs      # 🆕 DispatchTask impl Dispatchable
```

---

## Reference Files

### fafo 关键文件
- `fafo/exepipe/src/lib.rs` — ExePipe 入口（run_block / run_block_with_sim）
- `fafo/exepipe/src/simulator/` — 预执行 CrwSets 提取
- `fafo/exepipe/src/framer.rs` — 分帧（ParaBloom + flush_frame）
- `fafo/exepipe/src/dispatcher/dispatcher.rs` — Dispatcher 核心（add / handle_warmed / run_batch）
- `fafo/exepipe/src/dispatcher/dashboard.rs` — Dashboard（set_eei / set_executed / ignite_ll）
- `fafo/exepipe/src/dispatcher/dtask.rs` — DTask（warm_up / execute）
- `fafo/exepipe/src/context/block_context.rs` — BlockContext（execute_task / basic / storage_value）
- `fafo/exepipe/src/context/tx_context.rs` — TxContext（DatabaseRef adapter）
- `fafo/exepipe-common/src/statecache.rs` — StateCache（分片缓存）
- `fafo/exepipe-common/src/executor/execute_tx.rs` — 单笔交易执行

### reth 关键文件
- `crates/ethereum/payload/src/lib.rs` — 当前 payload builder（串行循环）
- `crates/evm/evm/src/execute.rs` — BlockBuilder trait + BasicBlockBuilder
- `crates/evm/evm/src/lib.rs` — ConfigureEvm + builder_for_next_block
- `crates/revm/src/database.rs` — StateProviderDatabase (DatabaseRef impl)
- `crates/revm/src/cached.rs` — CachedReads / CachedReadsDbMut
- `crates/xlayer/qmdb-provider/src/provider.rs` — QmdbStateProvider
- `crates/xlayer/parallel-exec/src/` — 已有并行执行基础模块
