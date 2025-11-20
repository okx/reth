# SetHead Command

## 概述

`sethead` 命令用于将区块链的规范链头重置到指定的区块号，类似于 Geth 的 `debug_setHead` 功能。

## 功能

- 将规范链头重置到指定的区块号
- 删除目标区块之后的所有规范区块
- 提供交互式确认以防止误操作
- 支持强制模式跳过确认

## 使用方法

### 基本用法

```bash
op-reth sethead <BLOCK_NUMBER>
```

### 示例

将链头重置到区块 1000：

```bash
op-reth sethead 1000
```

这将提示你确认操作：

```
⚠️  WARNING: This will reset the chain head from block 1500 to block 1000
   500 blocks will be removed from the canonical chain

Are you sure you want to continue? (y/N):
```

### 强制模式

如果你想跳过确认提示，可以使用 `--force` 或 `-f` 标志：

```bash
op-reth sethead 1000 --force
```

### 指定数据目录和链

```bash
op-reth --datadir /path/to/data --chain base-mainnet sethead 1000
```

## 参数

- `<BLOCK_NUMBER>`: 目标区块号（必需）
- `-f, --force`: 跳过确认提示（可选）

## 注意事项

⚠️ **警告**: 这是一个危险的操作！

- 此操作会永久删除目标区块之后的所有规范链数据
- 确保在执行前已经停止节点
- 建议在执行前备份数据库
- 此操作只删除 `CanonicalHeaders` 表中的条目，不会删除实际的区块数据

## 实现细节

该命令执行以下操作：

1. 验证目标区块是否存在于规范链中
2. 确定当前链头和目标区块之间的差异
3. 如果没有使用 `--force`，请求用户确认
4. 删除目标区块之后的所有 `CanonicalHeaders` 条目
5. 提交数据库事务

## 与 Geth 的 debug_setHead 的比较

此实现提供了与 Geth 的 `debug_setHead` RPC 方法类似的功能，但作为命令行工具：

- ✅ 将链头重置到指定区块
- ✅ 删除后续区块的规范链引用
- ✅ 简单直接的实现
- ⚠️ 需要停止节点才能运行（与 Geth 的运行时 API 不同）

## 错误处理

如果遇到以下情况，命令将返回错误：

- 目标区块在规范链中不存在
- 目标区块已经是或超过当前链头
- 数据库访问错误
- 用户取消操作

## 日志

该命令会记录以下信息：

- 目标区块详情
- 当前链头和要删除的区块数量
- 删除的规范头数量
- 操作成功完成

日志级别可以通过环境变量 `RUST_LOG` 控制。

