# reth 并行执行框架实现方案

## 设计目标

借鉴 fafo 的并行执行架构（Simulator → Framer → Dispatcher），在 `crates/xlayer/parallel-exec/` 下实现 reth 原生的并行交易执行框架。

**原则**：
- 复用 reth 已有基础设施（revm、StateProvider、BundleState、EthEvmConfig）
- 保持 QMDB + MDBX 双数据库架构不变
- 保留 reth 的跨块缓存能力（CanonicalInMemoryState）
- 不引入 fafo 作为依赖，只借鉴其设计

---

## 架构总览

```
                    reth Payload Builder
                           │
              ┌────────────▼─────────────┐
              │  收集交易 (BestTransactions) │
              └────────────┬─────────────┘
                           │ Vec<TransactionSigned>
              ┌────────────▼─────────────┐
              │  Simulator (N 分片)       │  预执行，提取读写集 CrwSets
              │  复用 reth EVM 执行       │  验证 nonce/balance，过滤无效交易
              └────────────┬─────────────┘
                           │ Vec<ExeTask> (带 CrwSets)
              ┌────────────▼─────────────┐
              │  Framer                   │  ParaBloom 冲突检测
              │                           │  无冲突交易分到同一 Frame
              └────────────┬─────────────┘
                           │ Vec<Frame>
              ┌────────────▼─────────────┐
              │  Dispatcher (64 线程)     │  Frame 间按依赖顺序
              │  rayon 线程池并行执行      │  Frame 内完全并行
              └────────────┬─────────────┘
                           │ Vec<ExecutionResult>
              ┌────────────▼─────────────┐
              │  合并 BundleState         │  按原始顺序收集结果
              │  计算 state root，组装区块 │  构建 receipts
              └────────────┘
```

### 三层缓存架构

```
读取优先级（从高到低）：

Layer 1: ParallelStateCache (并行安全，DashMap)
         ├── 当前块内并行执行产生的状态变更
         └── 上一个块的状态缓存（跨块热数据复用）

Layer 2: reth CanonicalInMemoryState (MemoryOverlayStateProvider)
         ├── 最近 N 个已执行但未持久化的块的状态
         └── 通过实现 revm DatabaseRef 暴露给并行执行线程

Layer 3: QMDB (SharedAdsWrap 无锁读) + MDBX (字节码/区块哈希)
         └── 持久化的全量状态，现有双数据库架构不变
```

**与串行模式的对比**：
```
串行模式:  revm CachedReads (单块) → MemoryOverlayStateProvider → QmdbStateProvider → QMDB/MDBX
并行模式:  ParallelStateCache      → MemoryOverlayStateProvider → QmdbStateProvider → QMDB/MDBX
                  ↑ 新增                      ↑ 复用                    ↑ 复用
```

---

## 模块与任务拆分

### 模块 1: CrwSets — 交易读写集

**目标**：定义读写集数据结构，为每笔交易提取其访问的账户和存储槽。

**文件**：`crates/xlayer/parallel-exec/src/crw_sets.rs`

**借鉴 fafo**：`fafo/exepipe-common/src/access_set.rs`

#### Task 1.1: CrwSets 数据结构
```rust
/// 10 字节短哈希，节省内存（fafo 用法一致）
type ShortHash = [u8; 10];

struct CrwSets {
    /// 读取的账户地址短哈希
    account_reads: Vec<ShortHash>,
    /// 写入的账户地址短哈希
    account_writes: Vec<ShortHash>,
    /// 读取的存储槽短哈希 (address + slot 混合哈希)
    storage_reads: Vec<ShortHash>,
    /// 写入的存储槽短哈希
    storage_writes: Vec<ShortHash>,
}
```
- 实现 `short_hash(address) -> ShortHash`
- 实现 `short_hash_slot(address, slot) -> ShortHash`

#### Task 1.2: 从 revm 执行结果提取 CrwSets
```rust
fn extract_crw_sets(result: &ResultAndState) -> CrwSets
```
- 从 `ResultAndState.state` (HashMap<Address, Account>) 中提取：
  - 被 touch 的账户 → account_reads/writes
  - 变更的 storage slot → storage_reads/writes
- 利用 revm Account 的 `status` 字段区分读/写

#### Task 1.3: 单元测试
- 简单转账的读写集
- ERC20 transfer 的读写集（应包含合约存储槽）
- 合约创建的读写集

**依赖**：无，可独立开发
**复杂度**：低
**预估**：1 天

---

### 模块 2: ParaBloom — 并行 Bloom Filter

**目标**：快速判断交易组之间是否存在读写冲突。

**文件**：`crates/xlayer/parallel-exec/src/para_bloom.rs`

**借鉴 fafo**：`fafo/exepipe/src/utils/para_bloom.rs`

#### Task 2.1: ParaBloom 核心实现
```rust
struct ParaBloom {
    /// 每个 Frame 两个 bloom filter: 读集 + 写集
    read_blooms: [BitArray; MAX_FRAMES],   // MAX_FRAMES = 64
    write_blooms: [BitArray; MAX_FRAMES],
}

impl ParaBloom {
    /// 将 CrwSets 添加到指定 Frame 的 bloom filter
    fn add(&mut self, frame_id: usize, crw_sets: &CrwSets);

    /// 返回冲突掩码：哪些 Frame 与此 CrwSets 冲突
    /// 冲突规则: 新读 vs 已写、新写 vs 已读、新写 vs 已写
    fn get_dep_mask(&self, crw_sets: &CrwSets) -> u64;

    /// 清空指定 Frame
    fn clear(&mut self, frame_id: usize);
}
```
- 5 个哈希函数从 ShortHash 派生
- 2^11 位 bloom filter（与 fafo 一致）

#### Task 2.2: 单元测试
- 无冲突交易 → 掩码为 0
- 读写冲突 → 正确检测
- 读读不冲突 → 掩码为 0
- 多 Frame 场景

**依赖**：模块 1（CrwSets 结构定义）
**复杂度**：低，纯数据结构
**预估**：1 天

---

### 模块 3: ParallelStateCache — 并行安全的状态缓存

**目标**：为并行执行线程提供线程安全的状态读写缓存（Layer 1）。

**文件**：`crates/xlayer/parallel-exec/src/state_cache.rs`

**借鉴 fafo**：`fafo/exepipe-common/src/statecache.rs`（分片 BytesCache）

#### Task 3.1: ParallelStateCache 结构
```rust
struct ParallelStateCache {
    /// 账户缓存：并行安全
    accounts: DashMap<Address, Option<Account>>,
    /// 存储缓存：并行安全
    storage: DashMap<(Address, StorageKey), Option<StorageValue>>,
    /// 字节码缓存
    bytecodes: DashMap<B256, Bytecode>,
}
```
- 使用 `DashMap`（或 fafo 风格的分片 RwLock）实现并发安全
- 支持 `None` 值表示"已查询但不存在"，避免重复穿透

#### Task 3.2: 实现 revm DatabaseRef trait
```rust
impl DatabaseRef for ParallelStateProvider {
    fn basic_ref(&self, address: &Address) -> Result<Option<AccountInfo>, Error> {
        // Layer 1: ParallelStateCache
        if let Some(cached) = self.cache.accounts.get(address) {
            return Ok(cached.clone());
        }
        // Layer 2+3: reth StateProvider (MemoryOverlay → QMDB/MDBX)
        let result = self.state_provider.basic_account(address)?;
        self.cache.accounts.insert(*address, result.clone());
        Ok(result)
    }

    fn storage_ref(&self, address: &Address, index: &U256) -> Result<U256, Error> {
        // 同样的三层查找
    }
}
```

#### Task 3.3: 跨块缓存复用
```rust
impl ParallelStateCache {
    /// 区块执行完毕后，保留热数据供下一个块使用
    fn rotate(&self) -> Self {
        // 保留当前缓存作为下一个块的只读缓存
        // 清除写集（因为已经提交到 BundleState）
    }
}
```

#### Task 3.4: 单元测试
- 并发读写正确性
- 缓存命中/穿透逻辑
- rotate 后数据可读

**依赖**：无，可独立开发
**复杂度**：中
**预估**：2-3 天

---

### 模块 4: Simulator — 预执行器

**目标**：并行预执行交易，提取 CrwSets，验证 nonce/balance，过滤无效交易。

**文件**：`crates/xlayer/parallel-exec/src/simulator.rs`

**借鉴 fafo**：`fafo/exepipe/src/simulator/`（4 分片并行，按 sender 路由）

#### Task 4.1: SimulatorShard 实现
```rust
struct SimulatorShard {
    /// 此分片追踪的 sender nonce
    nonce_map: HashMap<Address, u64>,
    /// reth EVM 配置（用于 dry-run）
    evm_config: EthEvmConfig,
}

impl SimulatorShard {
    /// 预执行单笔交易，提取 CrwSets
    fn simulate(&mut self, tx: &TransactionSigned, state: &dyn DatabaseRef) -> Option<SimResult> {
        // 1. 用 revm 执行交易（disable_nonce_check）
        // 2. 从 ResultAndState.state 提取 CrwSets（调用模块 1）
        // 3. 验证 nonce 递增
        // 4. 检查 balance 是否足够
        // 5. 返回 SimResult { tx, crw_sets } 或 None（无效交易）
    }
}
```

#### Task 4.2: Simulator 并行调度
```rust
struct Simulator {
    shards: Vec<SimulatorShard>,
    shard_count: usize, // 默认 4
}

impl Simulator {
    /// 按 sender 地址分配到分片，保证同一 sender 在同一分片内串行
    fn run(&mut self, txs: Vec<TransactionSigned>, state: &dyn StateProvider)
        -> Vec<SimResult>
    {
        // 1. 按 sender_address % shard_count 分组
        // 2. 每个分片在独立线程（rayon）执行
        // 3. 收集结果，按原始顺序合并
    }
}
```

#### Task 4.3: 集成 ParallelStateCache
- Simulator 预执行需要读取父区块状态
- 使用 ParallelStateCache 包装 reth StateProvider
- dry-run 的结果**不写入缓存**（只提取读写集，不改变状态）

**依赖**：模块 1（CrwSets）、模块 3（ParallelStateCache）
**复杂度**：中
**预估**：3-4 天

---

### 模块 5: ExeTask + Framer — 任务定义与分帧

**目标**：将带 CrwSets 的交易分组为无冲突的 Frame。

**文件**：
- `crates/xlayer/parallel-exec/src/task.rs`
- `crates/xlayer/parallel-exec/src/framer.rs`

**借鉴 fafo**：`fafo/exepipe/src/exetask.rs`、`fafo/exepipe/src/framer.rs`

#### Task 5.1: ExeTask 定义
```rust
struct ExeTask {
    /// 此 Task 内的交易（无冲突，可并行）
    txs: Vec<TransactionSigned>,
    /// 每笔交易的读写集
    crw_sets: Vec<CrwSets>,
    /// 合并后的读写集（用于 Bloom filter 查询）
    merged_access: CrwSets,
    /// 在原始交易列表中的起始位置（用于结果排序）
    original_index: usize,
}
```

#### Task 5.2: Framer 分帧逻辑
```rust
struct Framer {
    bloom: ParaBloom,
    frames: Vec<Vec<ExeTask>>,
    max_frames: usize, // 64
}

impl Framer {
    /// 将 SimResult 分配到 Frame
    fn add(&mut self, sim_result: SimResult) {
        let mask = self.bloom.get_dep_mask(&sim_result.crw_sets);
        let frame_id = mask.trailing_ones() as usize;  // 第一个不冲突的 Frame

        if frame_id >= self.max_frames {
            // 所有 Frame 都冲突，flush 最早的 Frame
            self.flush_oldest();
        }

        self.frames[frame_id].push(sim_result.into_task());
        self.bloom.add(frame_id, &sim_result.crw_sets);
    }

    /// 输出所有 Frame
    fn finish(self) -> Vec<Frame> { ... }
}

struct Frame {
    tasks: Vec<ExeTask>,
    /// 依赖的前序 Frame 索引列表
    depends_on: Vec<usize>,
}
```

#### Task 5.3: 单元测试
- 无冲突交易全部分到同一 Frame
- 冲突交易分到不同 Frame
- Frame 满时 flush 逻辑

**依赖**：模块 1（CrwSets）、模块 2（ParaBloom）
**复杂度**：中
**预估**：2-3 天

---

### 模块 6: Dispatcher — 并行调度器

**目标**：管理 rayon 线程池，按 Frame 依赖关系调度并行执行。

**文件**：`crates/xlayer/parallel-exec/src/dispatcher.rs`

**借鉴 fafo**：`fafo/exepipe/src/dispatcher/`（Dashboard 依赖图 + rayon 线程池）

#### Task 6.1: 简化版 Dispatcher（MVP）
```rust
struct Dispatcher {
    thread_pool: rayon::ThreadPool, // 64 线程
}

impl Dispatcher {
    /// Frame 间串行，Frame 内并行
    fn execute(
        &self,
        frames: Vec<Frame>,
        state_cache: &ParallelStateCache,
        evm_config: &EthEvmConfig,
    ) -> Vec<TaskResult>
    {
        let mut all_results = Vec::new();

        for frame in frames {
            // Frame 内的 Task 并行执行
            let frame_results: Vec<TaskResult> = self.thread_pool.install(|| {
                frame.tasks.par_iter().map(|task| {
                    self.execute_task(task, state_cache, evm_config)
                }).collect()
            });

            // 将 Frame 的结果应用到 state_cache（为后续 Frame 可见）
            for result in &frame_results {
                state_cache.apply_result(result);
            }

            all_results.extend(frame_results);
        }

        all_results
    }

    /// 执行单个 Task（单线程内，顺序执行 Task 内的交易）
    fn execute_task(
        &self,
        task: &ExeTask,
        state_cache: &ParallelStateCache,
        evm_config: &EthEvmConfig,
    ) -> TaskResult
    {
        // 使用 reth 的 EVM 执行交易
        // 状态读取通过 ParallelStateCache（三层缓存）
        // 收集 ResultAndState
    }
}
```

#### Task 6.2: 进阶版 — Dashboard 依赖图（可选，后续优化）
- 参考 fafo 的 Dashboard：原子链表追踪 Task 间依赖
- EEI（Earliest Execution Index）优化：允许跨 Frame 的无冲突 Task 提前执行
- Warmup 线程池：预热数据到缓存

**依赖**：模块 3（ParallelStateCache）、模块 5（Frame/ExeTask）
**复杂度**：中（MVP），高（进阶版）
**预估**：3-4 天（MVP）

---

### 模块 7: ResultCollector — 结果收集与合并

**目标**：将并行执行的结果按原始交易顺序合并为 reth 的 BundleState + Receipt。

**文件**：`crates/xlayer/parallel-exec/src/result_collector.rs`

#### Task 7.1: 按原始顺序排序结果
```rust
fn collect_results(task_results: Vec<TaskResult>) -> Vec<TxExecutionResult> {
    // 按 ExeTask.original_index 排序
    // 展开 Task 内的多笔交易结果
    // 输出按原始交易顺序排列的结果列表
}
```

#### Task 7.2: 合并为 BundleState
```rust
fn merge_to_bundle_state(results: &[TxExecutionResult]) -> BundleState {
    // 遍历所有 ResultAndState.state
    // 累积账户变更、存储变更、合约字节码
    // 处理同一地址的多次修改（后覆盖前）
    // 利用 revm 的 BundleRetention 机制或手动合并
}
```

#### Task 7.3: 构建 Receipt 列表
```rust
fn build_receipts(
    results: &[TxExecutionResult],
    txs: &[TransactionSigned],
) -> Vec<Receipt> {
    let mut cumulative_gas = 0;
    let mut log_index = 0;
    results.iter().zip(txs).map(|(result, tx)| {
        cumulative_gas += result.gas_used;
        let receipt = Receipt {
            tx_type: tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used: cumulative_gas,
            logs: result.logs.iter().map(|log| {
                // 重新编号 log_index（并行执行后 log 顺序可能混乱）
                let indexed_log = log.with_index(log_index);
                log_index += 1;
                indexed_log
            }).collect(),
        };
        receipt
    }).collect()
}
```

**依赖**：模块 6（Dispatcher 输出）
**复杂度**：中。BundleState 合并规则和 receipt 编号需要严格正确。
**预估**：2-3 天

---

### 模块 8: ParallelBlockBuilder — 并行区块构建器

**目标**：将以上模块组装为完整的并行区块构建流程，替换串行执行循环。

**文件**：`crates/xlayer/parallel-exec/src/builder.rs`

#### Task 8.1: ParallelBlockBuilder 实现
```rust
struct ParallelBlockBuilder {
    simulator: Simulator,
    dispatcher: Dispatcher,
    /// 跨块复用的状态缓存
    prev_cache: Option<Arc<ParallelStateCache>>,
}

impl ParallelBlockBuilder {
    fn build_block(
        &mut self,
        txs: Vec<TransactionSigned>,
        state_provider: StateProviderBox,  // MemoryOverlay → QMDB/MDBX
        evm_config: &EthEvmConfig,
        block_env: &BlockEnv,
    ) -> Result<ParallelBlockResult> {
        // 1. 创建 ParallelStateCache（Layer 1），包装 state_provider（Layer 2+3）
        let cache = ParallelStateCache::new(state_provider, self.prev_cache.take());

        // 2. Simulator 预执行，提取 CrwSets
        let sim_results = self.simulator.run(txs, &cache);

        // 3. Framer 分帧
        let mut framer = Framer::new();
        for result in sim_results {
            framer.add(result);
        }
        let frames = framer.finish();

        // 4. Dispatcher 并行执行
        let task_results = self.dispatcher.execute(frames, &cache, evm_config);

        // 5. 收集结果
        let ordered_results = collect_results(task_results);
        let bundle_state = merge_to_bundle_state(&ordered_results);
        let receipts = build_receipts(&ordered_results, &txs);

        // 6. 保留缓存供下一个块
        self.prev_cache = Some(Arc::new(cache));

        Ok(ParallelBlockResult { bundle_state, receipts, total_gas, total_fees })
    }
}
```

#### Task 8.2: Fallback 机制
- 并行执行失败时回退到 reth 原有的串行执行
- 通过 `PARALLEL_EXEC_ENABLED` 环境变量或配置项控制

**依赖**：模块 4, 5, 6, 7（所有前置模块）
**复杂度**：高。组装点，需要处理完整的执行生命周期。
**预估**：2-3 天

---

### 模块 9: Payload Builder 集成

**目标**：将 ParallelBlockBuilder 接入 reth 的 payload 构建流程。

**文件**：修改 `crates/ethereum/payload/src/lib.rs` 或创建 `crates/xlayer/parallel-exec/src/payload.rs`

#### Task 9.1: 替换串行执行循环
当前串行代码（`crates/ethereum/payload/src/lib.rs:226-364`）：
```rust
while let Some(pool_tx) = best_txs.next() {
    builder.execute_transaction(tx)?;  // 逐笔串行
}
```

替换为：
```rust
// 收集交易
let txs = collect_transactions(&mut best_txs, block_gas_limit);

// 并行执行
let result = parallel_builder.build_block(
    txs,
    state_provider,
    &evm_config,
    &block_env,
)?;

// 使用并行执行的结果构建区块
let bundle_state = result.bundle_state;
let receipts = result.receipts;
```

#### Task 9.2: 在 reth-qmdb binary 中注册
- 修改 `bin/reth-qmdb/` 的节点构建逻辑
- 添加配置项：线程数、Frame 数、是否启用并行
- `ParallelBlockBuilder` 作为 payload builder 的内部组件

#### Task 9.3: 与 InsertExecutedBlock 快速路径集成
- 并行执行的 `BundleState` 打包到 `BuiltPayloadExecutedBlock`
- 确保 engine 走 InsertExecutedBlock 路径，避免重复执行

**依赖**：模块 8
**复杂度**：中。接口适配 + 配置管理。
**预估**：2-3 天

---

### 模块 10: 测试与基准

**文件**：`crates/xlayer/parallel-exec/tests/`、`crates/xlayer/parallel-exec/benches/`

#### Task 10.1: 正确性测试
- 简单转账：串行 vs 并行结果完全一致（BundleState、receipts、state_root）
- ERC20 transfer：合约存储变更正确
- 合约创建：字节码正确
- 冲突交易：两笔交易操作同一存储槽，验证最终状态正确
- 混合负载：转账 + 合约调用 + 合约创建

#### Task 10.2: 性能基准
- 串行 vs 并行 TPS 对比（simple transfer）
- 不同交易数量（1k, 10k, 100k, 500k）下的 scalability
- 不同冲突率（0%, 10%, 50%, 100%）下的性能退化
- 三层缓存命中率统计

#### Task 10.3: 压力测试
- 长时间运行的稳定性（100 个块连续执行）
- 内存使用监控（StateCache 是否持续增长）

**依赖**：模块 9
**复杂度**：中
**预估**：3-4 天

---

## 开发顺序

```
Phase A — 基础设施（可并行开发，无依赖）:
  ├── 模块 1: CrwSets 读写集           [1 天]
  ├── 模块 2: ParaBloom 冲突检测       [1 天]
  └── 模块 3: ParallelStateCache       [2-3 天]

Phase B — 核心组件:
  ├── 模块 4: Simulator 预执行         [3-4 天]  (依赖模块 1, 3)
  ├── 模块 5: ExeTask + Framer 分帧   [2-3 天]  (依赖模块 1, 2)
  └── 模块 6: Dispatcher 并行调度      [3-4 天]  (依赖模块 3, 5)

Phase C — 集成:
  ├── 模块 7: ResultCollector 结果合并 [2-3 天]  (依赖模块 6)
  ├── 模块 8: ParallelBlockBuilder     [2-3 天]  (依赖模块 4-7)
  └── 模块 9: Payload Builder 集成     [2-3 天]  (依赖模块 8)

Phase D — 验证:
  └── 模块 10: 测试与基准              [3-4 天]  (依赖模块 9)
```

**依赖图**：
```
模块1 ──┬──→ 模块4 ──────────────┐
        ├──→ 模块5 ──→ 模块6 ──┤
模块2 ──┘                       ├──→ 模块7 ──→ 模块8 ──→ 模块9 ──→ 模块10
模块3 ──→ 模块4                 │
        ──→ 模块6 ──────────────┘
```

**总预估**：4-5 周

---

## 关键设计决策

### 1. 复用 reth 的 EVM 执行（而非 fafo 的自定义 handler）
fafo 使用自定义 `MpexHandler` 跳过 `reward_beneficiary()`。
reth 中直接使用 `EthEvmConfig` + `BlockBuilder::execute_transaction()`，保持与串行模式一致的 EVM 行为。

### 2. 输出 reth 原生类型
- 直接输出 `BundleState`（而非 fafo 的 `ChangeSet`）
- 直接输出 `Receipt`（而非从 `ResultAndState` 后处理）
- 无需适配层，减少 bug 风险

### 3. MVP 先行：Frame 级串行 + Frame 内并行
Dispatcher 初版不实现 fafo 的 Dashboard/EEI 优化。
先实现 Frame 间串行、Frame 内并行的简化版，验证正确性后再优化。

### 4. ParallelStateCache 使用 DashMap
fafo 使用自定义分片 BytesCache（128 分片）。
reth 中使用 `DashMap`（成熟库，已被 reth 其他组件使用），降低实现复杂度。

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Simulator 预执行开销 | 低冲突场景下预执行是浪费 | 可配置跳过 Simulator，直接按 sender 分组（MVP 方案） |
| revm 不是线程安全的 | 并行执行需要每线程独立 EVM 实例 | 每个 Task 在执行时创建新 EVM（revm 本身就是轻量的） |
| BundleState 合并顺序 | 同一地址多次修改，顺序错误导致状态错误 | 严格按 original_index 排序后合并 |
| StateCache 内存膨胀 | 长时间运行 OOM | rotate 时清理过旧数据；监控内存指标 |
| Bloom filter 误报 | 不冲突的交易被错误地分到不同 Frame | 接受误报（影响并行度但不影响正确性）；可选精确冲突检测 |

---

## 文件结构

```
crates/xlayer/parallel-exec/
├── Cargo.toml
├── src/
│   ├── lib.rs                 # 模块导出
│   ├── crw_sets.rs            # 模块 1: 读写集
│   ├── para_bloom.rs          # 模块 2: 并行 Bloom Filter
│   ├── state_cache.rs         # 模块 3: 并行安全状态缓存
│   ├── simulator.rs           # 模块 4: 预执行器
│   ├── task.rs                # 模块 5: ExeTask 定义
│   ├── framer.rs              # 模块 5: 分帧器
│   ├── dispatcher.rs          # 模块 6: 并行调度器
│   ├── result_collector.rs    # 模块 7: 结果收集与合并
│   ├── builder.rs             # 模块 8: 并行区块构建器
│   └── payload.rs             # 模块 9: Payload Builder 集成
├── tests/
│   ├── correctness.rs         # 串行 vs 并行一致性测试
│   └── integration.rs         # 端到端集成测试
└── benches/
    └── parallel_exec.rs       # 性能基准
```

---

## 参考文件

### fafo（借鉴设计，不引入依赖）
| 组件 | fafo 文件 | reth 对应 |
|------|-----------|-----------|
| CrwSets | `exepipe-common/src/access_set.rs` | `parallel-exec/src/crw_sets.rs` |
| ParaBloom | `exepipe/src/utils/para_bloom.rs` | `parallel-exec/src/para_bloom.rs` |
| StateCache | `exepipe-common/src/statecache.rs` | `parallel-exec/src/state_cache.rs` |
| Simulator | `exepipe/src/simulator/` | `parallel-exec/src/simulator.rs` |
| ExeTask | `exepipe/src/exetask.rs` | `parallel-exec/src/task.rs` |
| Framer | `exepipe/src/framer.rs` | `parallel-exec/src/framer.rs` |
| Dispatcher | `exepipe/src/dispatcher/` | `parallel-exec/src/dispatcher.rs` |
| ExePipe | `exepipe/src/lib.rs` | `parallel-exec/src/builder.rs` |

### reth（复用的基础设施）
| 组件 | 文件 |
|------|------|
| 串行 Payload Builder | `crates/ethereum/payload/src/lib.rs` (L226-364) |
| EthEvmConfig | `crates/evm/evm/src/lib.rs` |
| CanonicalInMemoryState | `crates/chain-state/src/in_memory.rs` |
| MemoryOverlayStateProvider | `crates/chain-state/src/memory_overlay.rs` |
| QmdbStateProvider | `crates/xlayer/qmdb-provider/src/provider.rs` |
| QmdbStore | `crates/xlayer/qmdb-provider/src/store.rs` |
| BundleState | `revm` crate (reth 依赖) |
