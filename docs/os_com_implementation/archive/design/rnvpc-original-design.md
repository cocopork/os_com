> **Archived:** original date: 2026-09-22. **Archive reason:** superseded design history. **Citation status:** historical context only; not current implementation or validation evidence. **Replaced by:** [AchillesFS design](../../rNVPC-rTIO/design.md) and [implementation reference](../../rNVPC-rTIO/implementation.md).

# rNVPC 与 rNVPC-extend 设计

本文定义 rNVPC 与 rNVPC-extend 的权威概念、数据结构和行为边界。本文只描述目标设计；实验
实现映射见 [porting.md](rnvpc-porting.md)，历史路线差异见 [design-evolution.md](rnvpc-design-evolution.md)。

## 1. 总体定位

| 组件 | 定位 | PM 中的主要内容 | 主要用途 |
|---|---|---|---|
| rNVPC | Rust + PM 上的 WAL-protected PageCache | PageCache pages、per-inode WAL | 模拟传统持久 PageCache 的性能路径 |
| rNVPC-extend | 数据面持久、控制面易失的扩展 PageCache | NVPage、NVBH 及最小恢复描述 | 支持低开销运行时管理和 DRAM/NV 分层 |

二者都不建立全局 SSD 物理索引，也不把 NV 与 SSD 统一成一套物理地址空间。每个 page 的
descriptor 单独记录 SSD backing physical address。rTIO 在此基础上管理 DRAM/NV page 切换
和并发 writeback，见 [rTIO 设计](rtio-original-design.md)。

## 2. 术语

### 2.1 NVPage

NVPage 是驻留在持久内存中的文件数据页，基本单位为 4 KiB。一个 NVPage 保存页面内容以及
识别、恢复和写回所需的最小持久描述。

### 2.2 NVBH

NVBH 是 **Non-Volatile Buffer Head**，表示小于或等于一个文件系统块的持久元数据缓存单元。
名称复用 Linux EXT4 使用 `buffer_head` 管理 metadata block 的习惯。

旧文档中使用 `Frame` 或 `NVFrame` 表示元数据对象的地方，统一改为 NVBH。只有在描述底层
物理承载容器时才允许使用 frame，不再用 frame 指代文件系统元数据对象。

### 2.3 Runtime handle

Runtime handle 包括 B-tree 节点、Rust 引用、锁、pin/activity count、容量计数器、free list、
worker 状态和队列节点。它们只存在于 DRAM，掉电后全部丢失，不属于恢复输入。

## 3. rNVPC：WAL-protected PM PageCache

### 3.1 设计目标

rNVPC 假设采用类似 Corundum 的方法，在 Rust + PM 上实现持久 PageCache，并使用 WAL 保护
PageCache 操作的一致性。本项目不实现完整 Corundum，也不以该模拟实现证明崩溃一致性；它只
模拟以下性能成本：

1. WAL append；
2. update 的 Copy-on-Write；
3. WAL 对 PM 容量的占用；
4. WAL 满时由 writeback 造成的回压。

实验实现复用 Asterinas 的 DRAM PageCache，将 PM page、persist 和恢复过程抽象为内存操作和
计数器。结果只能用于比较软件路径开销，不能表述为真实 PM 延迟、带宽或掉电恢复结果。

### 3.2 Per-inode WAL

每个 inode 独立拥有一段定长 WAL：

```text
WAL_CAPACITY_PER_INODE = 1024 op-log records
```

WAL 占用计入 rNVPC 的 PM 容量。一个 inode 的 WAL 额度不能被其他 inode 借用；因此一个 inode
的日志耗尽只阻塞该 inode 的新 update，不直接阻塞仍有额度的 inode。

op-log 只需保存性能模拟所需的最小字段，例如操作类型、页索引、旧/新版本标识和状态。模拟器
不需要实现可重放的通用事务日志，也不需要覆盖全部文件系统 namespace 操作。

### 3.3 Update 路径

PageCache update 采用 CoW 模拟：

```text
1. 锁定目标 inode 的 update 上下文
2. 从该 inode 的 WAL 预留一个 op-log
3. 追加 WAL record，并计入 PM 使用量
4. 分配新 page，复制旧内容并应用 update
5. 将 PageCache 索引切换到新 page
6. 旧 page 与 op-log 等待 writeback 后回收
```

不能在未获得 WAL 额度时修改 PageCache。若 WAL 已有 1024 条未退休记录，rNVPC 触发该 inode
的 writeback，并等待至少一个 op-log 可回收后再继续。实现以简单、稳定、可测量为目标，不模拟
复杂的 group commit、日志压缩或跨 inode transaction。

### 3.4 Writeback 与 WAL 回收

writeback 将该 inode 的脏 PageCache pages 提交到 SSD。对应 page 完成后，模拟器退休覆盖这些
page 的 op-log，并释放其 PM 额度。WAL 回收依据完成边界推进，不能仅因 BIO 已提交就释放日志
额度。

rNVPC 的 writeback 只承担 WAL 空间回收和数据落盘；它不使用全局物理索引，也不跨 inode
寻找连续 SSD 地址。

## 4. rNVPC-extend：持久数据面与 DRAM 控制面分离

### 4.1 设计目标

rNVPC-extend 保留 NV 中的数据与元数据对象，把运行时控制结构移到 DRAM：

```text
Persistent data plane
  - NVPage contents
  - NVBH contents
  - per-object recovery descriptor
  - per-page SSD backing physical address

Volatile control plane
  - B-tree lookup index
  - Rust handles and pointers
  - locks and pin/activity counts
  - capacity counters and free lists
  - dirty queues and worker ownership
```

这种分离避免在 PM 中持久化锁、引用计数或 Rust 指针。重启后扫描 NV 数据面，使用安全的 Rust
API 重建 B-tree、计数器和 runtime handles。

### 4.2 DRAM B-tree 索引

rNVPC-extend 在 DRAM 中维护类似 PageCache 的 B-tree。逻辑 key 定义为：

```text
PageKey = (filesystem_id, inode_id, page_index)
```

叶节点保存带显式 tier 标记的运行时 descriptor：

```text
RuntimePageDescriptor {
    key,
    residency: Dram | Nv,
    runtime_handle,
    ssd_backing_block,
    runtime_state,
}
```

B-tree 可以索引 NVPage，也可以分配并索引 DRAM page。DRAM page 必须带 `Dram` 标记；其 raw
pointer、frame address 和引用只存在于 DRAM。NV 数据面不得保存重启后会被解释为有效地址的
DRAM pointer。

恢复时丢弃全部 DRAM entries，只扫描 NVPage/NVBH 的持久描述并重建 `Nv` entries。DRAM clean
pages 是纯缓存，掉电丢失不影响恢复语义。

### 4.3 Per-page SSD 地址

每个 NVPage/NVBH descriptor 记录自己的 SSD backing physical address。该地址只用于对象自身
的 writeback，不建立以下全局映射：

```text
SSD physical block -> resident NV object
```

候选选择不以物理地址排序。局部 writeback 在已经选定一个 inode 后，可以按该 inode 内的逻辑
页顺序构造 BIO，并自然合并碰巧连续的 SSD blocks；这种合并不是全局调度策略。

## 5. rNVPC-extend 的 soft-update 写路径

> **实现状态（2026-09-24）：曾偏离本节设计。**
> I3（`2126b6d3c`）把 **rNVPC 的 CoW + per-inode WAL** 整套复用到 `rnvpc_extend`，
> 只额外调了一次 `extend_publish_residency` ⇒ soft update 与 WAL 并存，WAL 成为纯开销
> （1/4 缩放实测占单页退休成本 **64%**）。
> **本节是 rNVPCe 的唯一权威设计：rNVPCe 不使用 WAL，一致性由 soft update 提供。**
> 纠正任务见 `../ext4-io-integration.md` §1.1 / §1.2 的 **I5**。

### 5.1 写入原则

应用 write 后立即为本批次准备所需 NVPage 与 NVBH。数据和元数据在 NV tier 内仍按 soft-update
依赖顺序持久化：

```text
1. 为数据页和引用更新预留 NV 容量
2. 分配新的 NVPage 和必要的 NVBH
3. 写入并完成 NVPage 数据
4. 更新 NVBH，但尚不发布未就绪的引用
5. 在 NVPage 有效后发布 NVBH 中的 page reference
6. 持久化使新状态可达的上层元数据
7. 撤销旧引用后，旧 NVPage/NVBH 才可回收
```

核心不变量：

1. NVBH 对 NVPage 的新引用不得先于 NVPage 数据有效；
2. 旧 NVPage 不得在持久引用撤销前回收；
3. 已发布对象的 update 使用 CoW，不原地破坏旧的可恢复版本；
4. 同 inode 的 publish 串行，不同 inode 可以并行；
5. 容量等待发生在取得低层 page/NVBH lock 之前。

这里的 soft update 是 NVPage 与 NVBH 之间的局部依赖协议，不意味着恢复全局 SSD 物理顺序，也
不要求建立通用 journal。

### 5.2 DRAM/NV page 状态

最小运行时状态为：

```text
DramClean
    只读/干净缓存，可随时丢弃

NvDirty
    NV 是当前数据源，等待 SSD writeback

NvWriteback
    SSD BIO 已提交，NVPage 仍必须保留

NvReclaimable
    SSD writeback 已成功完成，NVPage 可以回收
```

DRAM clean page 被修改时，写路径分配新的 NVPage，通过 CoW 生成新版本并将 B-tree entry 切换
为 `NvDirty`。原 DRAM page 随后可丢弃。

NVPage writeback 成功后只标记为 `NvReclaimable` 并回收，不复制或转换为 DRAM page。后续读取
若没有缓存命中，则从 SSD 读取并分配新的 `DramClean` page。

## 6. Writeback 边界

rNVPC-extend 只向 rTIO 暴露 inode 级候选、每页 SSD target、page 状态和容量水线。rTIO 的
worker 以 inode 为所有权粒度并发写回，详见 [rTIO 设计](rtio-original-design.md)。

writeback completion 必须满足：

1. SSD 写成功前，NVPage/NVBH 不得进入 reclaimable；
2. completion 失败时保留 NV 数据和原状态，供后续重试；
3. 成功完成的数据 page 直接回收，不迁移为 DRAM page；
4. 元数据 writeback 继续遵守对象内部依赖，但不进行跨 inode 物理排序。

## 7. 恢复

rNVPC-extend 恢复只依赖持久数据面：

```text
1. 扫描 NVPage/NVBH 描述
2. 校验对象类型、状态和引用
3. 重建 DRAM B-tree 的 Nv entries
4. 重建容量计数器、free list 和 inode 候选集合
5. 丢弃所有崩溃前的 DRAM page、锁、handle 和 worker 状态
6. 将崩溃时的 writeback 中间态恢复为可重试状态
```

服务恢复与空间回收可以分开：完成必要索引和根对象重建后即可提供服务，不可达对象可由后台
扫描回收。

## 8. 不变量

1. 持久对象中不存在 Rust pointer、锁或运行期引用计数。
2. DRAM page 永不成为掉电恢复所需的唯一副本。
3. NVPage 数据有效先于 NVBH 新引用发布。
4. 持久撤销引用先于旧 NVPage/NVBH 回收。
5. 每个 page 自己记录 SSD backing address，不存在全局物理索引。
6. writeback completion 成功后 NVPage 可回收，但不转换成 DRAM page。
7. B-tree 和全部运行时 handles 均可通过扫描持久数据面重建。
8. rNVPC 模拟器的 WAL 固定为每 inode 1024 条，并计入 PM 容量。
9. rNVPC update 必须先获得 WAL 额度，再执行 CoW PageCache update。
10. rNVPC 的性能模拟结果不用于证明真实 PM 崩溃一致性。

## 9. 非目标

- 不实现完整 Corundum；
- 不用 DRAM 模拟器证明真实 PM 掉电正确性；
- 不持久化 Rust runtime control objects；
- 不建立全局 SSD 物理索引；
- 不跨 inode packing 或按全局物理连续性调度；
- 不在 writeback 后把 clean NVPage 转换为 DRAM page；
- 不让后台 writeback 决定已返回 write 的运行期可见性。
