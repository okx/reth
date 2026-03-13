# sei-db Rust 与 Go 功能对比与缺口分析

> 基于对 Go 版 sei-db（`megaEth/sei-chain/sei-db`）与 Rust 版（`crates/xlayer/sei-db`）的逐项对比，列出**会影响迁移后 Rust 版 DB 使用**的未实现或未接入功能。

---

## 一、会影响实际使用的缺口

### 1. **Composite Importer（State Sync 恢复）— 未接入**

| 项目 | Go | Rust | 影响 |
|------|-----|------|------|
| **CompositeCommitStore.Importer(version)** | 返回 `SnapshotImporter(cosmosImporter, evmImporter)`，用于 state sync 恢复时把快照数据写入 SC（cosmos memiavl + evm flatkv） | `create_importer(version)` 直接返回 `Err("composite importer not yet implemented")`，未组装 cosmos + evm | **若启用 state sync 恢复（从快照还原节点），会失败** |

**细节：**

- Rust 侧 **SnapshotImporter**（`seidb-sc/src/composite/importer.rs`）逻辑已实现：按 module 路由、evm 只收 leaf（height==0），与 Go 一致。
- **未做**的是在 `CompositeCommitStore::create_importer()` 里构造并返回该 importer：
  - 需要拿到 cosmos 的 importer（MemiavlCommitStore 当前 trait 的 `importer()` 也返回 `Err`，见下）。
  - 若有 evm_committer，用 FlatKV 的 `KvImporter` 作为 evm importer。
  - 返回 `Box::new(SnapshotImporter::new(cosmos_importer, evm_importer))`。

**调用链（Go）：**  
`storev2/rootmulti/store.go` 的 `restore()` → `rs.scStore.Importer(height)` → 从 proto 读 `SnapshotItem`，对每个 store 调用 `AddModule`，对每个 IAVL 节点调用 `AddNode`，最后 `Close()`。  
Rust 若要在 reth/xlayer 中支持同一 state sync 恢复流程，必须让 composite 的 `importer(version)` 返回可用的 composite importer。

---

### 2. **MemiavlCommitStore 的 Importer（Cosmos 侧 State Sync）— 未实现**

| 项目 | Go | Rust | 影响 |
|------|-----|------|------|
| **CommitStore.Importer(version)**（仅 cosmos / memiavl） | 返回 `MultiTreeImporter`，可 `AddModule`/`AddNode`/`Close` 将 state sync 的节点流写入 memiavl | `MemiavlCommitStore` 的 `importer()` 返回 `Err("importer not implemented yet")` | 即使 composite 想组装 importer，也没有可用的 cosmos 端 importer |

**细节：**

- Go 的 `MultiTreeImporter` / `TreeImporter` 接收 `types.SnapshotNode`（Key, Value, Height, Version），写入临时目录后完成导入。
- Rust 的 `TreeImporter`（`memiavl/import_export.rs`）面向的是 `ExportNode` 和目录，与 state sync 的 `ScSnapshotNode` 流不是同一抽象；需要一层适配或单独实现一个“按 SnapshotNode 流写入 memiavl”的 importer，并挂到 `MemiavlCommitStore::importer()` 上。

---

### 3. **FlatKV 的 trait Importer — 故意未暴露**

| 项目 | Go | Rust | 影响 |
|------|-----|------|------|
| **FlatKV CommitStore.Importer(version)** | 返回 `KVImporter`，用于 state sync 时写入 FlatKV | trait 实现返回 `Err("importer via trait not implemented; use KvImporter directly")` | 通过 **trait** 拿不到 FlatKV importer；直接使用 `flatkv::importer::KvImporter` 可用，且逻辑完整 |

**结论：** FlatKV 底层导入能力已实现（`KvImporter`），只是未通过 `Committer::importer()` 暴露。若在 composite 里用 `evm_committer` 做 evm 导入，需要以 `KvImporter::new(store, version)` 方式使用，而不是通过 trait。

---

## 二、已实现或已等价的功能（澄清 deferred 文档）

| 文档中的项 | 说明 | Rust 现状 |
|------------|------|-----------|
| **BuildMemIAVLEVMKey / 迭代器适配** (T9.4.0e) | 迭代器 Key() 返回 memiavl 格式 key | 已实现：`seidb_common::evm_keys::build_memiavl_evm_key`，FlatKvDbIterator 的 `convert_to_memiavl` + `cached_memiavl_key` 用于导出时统一 key 格式 |
| **FlatKV importer 底层** | 将 state sync 的 leaf 写入 FlatKV | 已实现：`seidb-sc/src/flatkv/importer.rs` 的 `KvImporter`（add_node、flush、close、commit_global_metadata）与 Go 行为一致 |
| **异步 ChangeSet 真并发** (T9.4b.1) | 文档称“后来已实现” | 以当前代码为准即可；若已实现，则无缺口 |
| **Pruning** | SS 定期 prune 旧版本 | 已实现：`seidb-ss/src/pruning.rs` 的 `PruningManager` 与 Go 的 prune loop 一致。测试里的 `unimplemented!()` 仅在 **MockStateStore** 的 iterator/reverse_iterator，不影响生产 |

---

## 三、对迁移/使用影响较小的未实现项

以下项在 deferred 文档中有记录，对**当前**迁移后“能跑、能写能读”的影响较小，可按需后续补全。

| 编号/项 | 说明 | 影响 |
|---------|------|------|
| **T9.4.0a** FlatKvIterator 独立 trait | 抽象层拆分 | 无：现有 `DbIterator` 已满足使用 |
| **T9.4.0b** TOML 配置模板 | 运维配置生成 | 低：可手写配置 |
| **T9.4.0c/0d** PebbleMetrics / enableMetrics | DB 指标采集 | 生产监控有用；Rust 有 `seidb_common::metrics::db_metrics`（feature "metrics"），是否全链路接入需单独看 |
| **T9.4b.2 / T9.7.4** MultiTree 导入导出完整版 | 独立 MultiTreeImporter/Exporter API、state sync | 当前无 state sync 需求则可延后 |
| **T9.5.1 / T9.5.2** SequentialReadAndFillPageCache、快照写入限速 | 性能优化 | 大状态、高负载时有用，非正确性缺口 |
| **T9.7.1–T9.7.3** Parquet/DuckDB 真格式 | 大规模分析、SQL 查询 | 当前二进制格式功能等价，无分析需求可延后 |

---

## 四、总结：迁移后要保障“与 Go 行为一致”时建议补全的项

1. **Composite Importer 接入**  
   在 `CompositeCommitStore::create_importer(version)` 中：
   - 若存在 cosmos 的 importer（见下），则构造 cosmos importer；
   - 若存在 `evm_committer`，则用 `KvImporter::new(evm_committer, version)` 作为 evm importer；
   - 返回 `Box::new(SnapshotImporter::new(cosmos_importer, evm_importer))`。

2. **MemiavlCommitStore 的 Importer**  
   实现 `MemiavlCommitStore::importer(version)`，返回一个实现 `seidb_traits::sc::Importer` 的类型，内部接收 `ScSnapshotNode` 流（AddModule/AddNode/Close），写入当前 memiavl 目录/DB，与 Go 的 `MultiTreeImporter` + `TreeImporter` 行为一致。

完成上述两项后，通过 `Committer::importer(version)` 的 state sync 恢复路径即可与 Go 对齐；若 reth/xlayer 近期不使用 state sync，可只做标记，待需要时再补。

---

## 五、参考代码位置（Rust）

| 功能 | 文件/位置 |
|------|-----------|
| Composite create_importer 返回 Err | `seidb-sc/src/composite/store.rs` 约 238–241 行 |
| SnapshotImporter 实现 | `seidb-sc/src/composite/importer.rs` |
| MemiavlCommitStore importer 返回 Err | `seidb-sc/src/memiavl/trait_impl.rs` 约 162–164 行 |
| FlatKV KvImporter | `seidb-sc/src/flatkv/importer.rs` |
| BuildMemIAVLEVMKey 等价 | `seidb-common/src/evm_keys.rs`（`build_memiavl_evm_key`），FlatKvDbIterator 使用处：`seidb-sc/src/flatkv/iterator.rs` |
| PruningManager | `seidb-ss/src/pruning.rs` |
