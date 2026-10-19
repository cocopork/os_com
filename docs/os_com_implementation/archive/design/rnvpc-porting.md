> **Archived:** original date: 2026-09-22. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rNVPC 与 rNVPC-extend 实验实现映射

本文把 [design.md](rnvpc-original-design.md) 的目标设计映射到 Asterinas。它是后续实现约束，不是完成状态
报告：当前阶段只确定接口、数据结构与实验边界，不声称 rNVPC 或 rNVPC-extend 已经实现。

## 1. 实验目标

实验提供两个互斥模式：

| 模式 | 模拟对象 | 核心成本 |
|---|---|---|
| rNVPC | Rust + PM 的 WAL-protected PageCache | per-inode WAL、CoW、容量回压 |
| rNVPC-extend | 持久数据面、DRAM 控制面的扩展 PageCache | NVPage/NVBH、DRAM B-tree、soft update、分层切换 |

两种模式都复用 Asterinas 的 PageCache、block I/O 和 EXT4 对象，不实现真实 PM 驱动。实验数据只能
说明软件路径和调度策略的开销，不能证明掉电一致性或真实 PM 性能。

## 2. 复用与新增边界

| 子系统 | 处理方式 |
|---|---|
| Asterinas PageCache | 复用页面、查找、脏页与回写接口 |
| EXT4 inode/page mapping | 复用逻辑页到 SSD block 的映射 |
| block layer/BIO | 复用异步提交与 completion |
| rNVPC WAL | 新增轻量模拟层，不实现通用事务日志 |
| rNVPC-extend NVPage/NVBH | 从现有 NVPool 数据结构收敛而来 |
| DRAM B-tree/runtime handle | 新增；掉电后扫描 NV 数据面重建 |
| rTIO worker | 新增 inode 所有权和并发 writeback 调度 |

不移植以下旧路线：全局 SSD 物理索引、SSD/NV 统一物理编址、跨 inode 物理连续页优先调度，以及
把运行时锁、引用计数或 Rust pointer 持久化。

## 3. rNVPC 映射

### 3.1 Per-inode WAL

每个活跃 inode 建立一个 DRAM runtime handle，关联固定容量的 WAL 区域：

```text
const WAL_CAPACITY_PER_INODE: usize = 1024;

InodeWalRuntime {
    inode_id,
    head,
    tail,
    used,
    records[1024],
    waiters,
}
```

WAL records 和被 CoW page 占用的空间都计入模拟 PM 容量。记录至少包含 inode、page index、操作
类型、旧/新版本标识和生命周期状态。不同 inode 之间不共享 1024 条额度。

PageCache 修改前执行：

```text
reserve inode WAL record
  -> append op-log
  -> allocate and copy page
  -> apply update
  -> publish new PageCache version
```

WAL 满时，只唤醒或创建该 inode 的 writeback，并等待 completion 退休记录。不能在 BIO submit 时
提前释放额度。

### 3.2 CoW 模拟

即使底层 PageCache 原本允许原地修改，rNVPC 模式也要走新页分配与索引替换，以保留 CoW 的
内存分配、复制和引用切换成本。模拟器不需要重放 WAL；重启测试只检查状态清空与容量记账。

## 4. rNVPC-extend 映射

### 4.1 持久数据对象

| 对象 | 粒度 | 内容 |
|---|---|---|
| NVPage | 4 KiB | 文件数据、恢复标识、SSD backing block |
| NVBH | 小于或等于文件系统块 | EXT4 metadata、恢复标识、引用关系 |

旧代码中的 `Frame`/`NVFrame` 元数据语义统一改名为 `NVBH`。底层内存 frame 仍可作为物理承载
类型，但不能作为文件系统对象名称暴露。

### 4.2 DRAM 控制面

新增以 `(filesystem_id, inode_id, page_index)` 为 key 的 DRAM B-tree。叶节点保存：

```text
residency: Dram | Nv
runtime handle
SSD backing block
dirty/writeback/reclaim state
```

锁、pin/activity count、容量计数、free list、队列和 worker ownership 均只存在 DRAM。恢复流程扫描
NVPage/NVBH，重建 B-tree 的 `Nv` entries；崩溃前所有 `Dram` entries 直接丢弃。

### 4.3 写路径与 soft update

应用 write 首次修改一个 DRAM clean page 时，分配 NVPage 并执行 CoW。需要修改 EXT4 metadata
时立即分配相应 NVBH。发布顺序固定为：

```text
NVPage data ready
  -> NVBH 内容 ready
  -> NVBH 发布对 NVPage 的新引用
  -> 上层 metadata 发布新状态
  -> 撤销旧引用
  -> 旧对象可回收
```

同一 inode 内的 publish 串行，不同 inode 可以并行。容量预留必须发生在获取低层对象锁之前。

### 4.4 DRAM/NV 页面切换

- SSD read miss 分配 `DramClean` page。
- `DramClean` 被写时，通过 CoW 产生 `NvDirty`，并切换 B-tree entry。
- `NvDirty` 提交后为 `NvWriteback`。
- completion 成功后为 `NvReclaimable`，直接回收 NVPage。
- completion 失败时保留 NVPage 并重新排队。
- 不把已写回的 clean NVPage 转换为 DRAM page。

## 5. 与 rTIO 的接口

rNVPC-extend 向 rTIO 提供 inode 级候选和页级描述，不提供全局物理排序接口：

```text
claim_dirty_inode() -> InodeWritebackLease
collect_inode_pages(lease, budget) -> Vec<PageWritebackDesc>
mark_submitted(page, bio_id)
complete(page, result)
release_inode(lease)
```

`InodeWritebackLease` 保证同一时刻一个 inode 只由一个 worker 写回；多个 worker 可分别处理不同
inode。页描述包含自己的 SSD backing address，rTIO 可在 inode 内自然合并连续 block。

## 6. 建议实现阶段

| 阶段 | 内容 | 状态（2026-09-24） |
|---|---|---|
| **P0** | 名称与配置：`rnvpc`/`rnvpc_extend`/`rtio` 互斥开关；元数据对象改名 NVBH | ✅ 已实现 |
| **P1** | rNVPC 模拟：per-inode 1024-entry WAL、容量记账、CoW、WAL-full 回压 | ✅ 已实现 |
| **P2** | rNVPC-extend 数据面：NVPage/NVBH descriptor + per-page SSD 地址，删除全局物理索引 | ✅ 已实现 |
| **P3** | DRAM 控制面：B-tree、runtime handles、扫描重建、residency 标记 | ✅ 已实现 |
| **P4** | soft update：数据先于引用、撤销先于回收的持久顺序和错误回滚 | ✅ 已实现（池层原语 + 发布 API） |
| **P5** | rTIO：inode lease、多 worker 提交、completion 回收和水线触发 | 🚧 v0 已实现（单线程回刷 + 容量回压 + completion 回收）；多 worker 池待做 |
| **P6** | 实验：与普通 PageCache、单 worker、无 WAL、无 CoW 等基线消融比较 | 🚧 首轮数字已出 |

各阶段的实现细节与验收对比见 [../implementation-tasks.md](achillesfs-implementation-tasks.md)；
**当前实际待办是 I5**（`ext4-rNVPCe` 去 WAL 重做），见
[../I5-extend-cow-implementation.md](achillesfs-extend-implementation.md)。

## 7. 验证要求

### 7.1 功能与不变量

- WAL 第 1024 条可用，第 1025 条 update 触发该 inode 的 writeback 回压；
- 一个 inode WAL 满不阻塞仍有额度的其他 inode；
- WAL 和 CoW page 都计入 PM 容量；
- B-tree 可同时区分 `Dram` 与 `Nv` entries；
- 恢复后没有 DRAM pointer、旧锁或旧计数器被复用；
- NVBH 不得引用未 ready 的 NVPage；
- completion 前 NVPage 不可回收，失败 completion 不丢数据；
- 成功 writeback 后 NVPage 被回收，不发生 NV 到 DRAM 的复制；
- 同一 inode 不被两个 worker 同时 claim，不同 inode 可并行提交。

### 7.2 性能观测

至少记录：WAL append 延迟、CoW 分配/复制延迟、WAL-full 等待、每 inode dirty pages、worker
活跃度、in-flight BIO 数、submit-to-completion 延迟、BIO size、NV 占用和回收速率。

报告必须注明使用 DRAM 模拟 PM，并分别给出 rNVPC、rNVPC-extend 与原生 PageCache 的配置。

## 8. 实现状态

本文不记录“已完成”项。完成状态只应由具体提交、测试输出和实验目录证明。

当前快照（简表，细节以提交为准；见 [../README.md](../../rNVPC-rTIO/README.md) 与
[../NEXT-SESSION.md]）：

- `rnvpc` 池 P0–P5 **已实现并提交**（`os_com` 分支 `nvpool-port`）。
- `ext4` 接入：I1/I2（`ext4-rNVPC`）已完成，全量 ktest 零回归；
  **I3（`ext4-rNVPCe`）实现偏离 design.md §5（引入了 WAL）**，重做为 I5。
- rTIO v0 已落地（单线程 NV→SSD 回刷 + 池容量回压 + 旧页回收竞态修复）；
  多 worker 池仍待实现。

> 交付进度请以 [../NEXT-SESSION.md](../../STATUS.md)（交接单）为唯一入口，
> 本文件只描述设计约束。
