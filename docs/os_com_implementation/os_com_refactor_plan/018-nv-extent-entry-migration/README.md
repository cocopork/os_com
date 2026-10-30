# 018：以 NV extent entry 为单位的迁移与校验消除

日期：2026-10-30。状态：Implemented on `recovery/rnvpce-page-redirection`；聚焦正确性回归通过，性能验证待执行。

## 0. 实现结果

当前 Native rNVPCe 路径已经完成以下转换：

- 前台按逻辑页、NVPage slot、SSD block 三者连续性发布 `NvRedirectExtentRecord`，单个 entry 最多 128 页；
- 候选索引直接保存 entry，不再从逐页 redirect 动态拼接迁移范围；
- entry 使用 `Dirty -> Migrating -> IoComplete -> Reclaiming -> Reclaimed` 状态机，读者和写者在 entry 粒度协调；
- rTIO 从不可变 entry 范围直接构造 BIO，completion 不遍历 extent tree、不做 baseline token 或逐页身份比较；
- 回收先发布 `SsdAuthoritativePendingFree`，等待读者退出，只清除 head selector，然后释放完整 NVPage 范围；
- 镜像格式升级为 v8。一个 rNVPCe entry 只有 head slot 带持久化 selector/range record，tail NVPage 不再发布逐页 descriptor；
- 恢复扫描从 range record 一次重建完整 entry，`PendingFree` 记录只清除 head selector并回收完整范围；
- rNVPC WAL 和持久化元数据对象继续使用原有 descriptor 与 generation，不受本次格式语义调整影响。

运行期兼容索引目前仍为部分旧 API 提供 `PersistentObjectRef`，但 Native 迁移、completion、回收和恢复正确性不再读取数据页 `allocation_seq` 或逐页持久化 descriptor。后续若删除这些仅供旧测试/接口使用的 API，应作为独立清理提交，不能重新引入正常路径依赖。

当前剩余成本是 R3 的 DRAM allocator 更新：`ObjectTable` 仍采用 per-slot 锁，所以释放一个 N 页 entry 时仍需逐 slot 将运行期记录改为 free。该过程不再读取或校验持久化 descriptor，也不产生 NVDIMM selector 写放大；若性能分解确认它仍显著，应另行引入 range-owned data-slot table，使连续范围的 DRAM 释放也只获取一次锁。在没有该结构前，不能通过跳过 pin/slot-reuse 检查来假装完成批量释放。

## 1. 问题

当前 rNVPCe 候选索引先保存逐页 redirect，再根据逻辑页、NVPage slot 和 SSD target
是否连续，动态构造 `DirtyMigrationExtent`。该对象不等同于 extent tree 中的一个
`ExtentRecordEntry`：索引可能合并两个相邻 entry，也可能在 redirect 索引分区边界处拆分一个
entry。

迁移和回收路径还为每个 NVPage 保存并反复校验 `allocation_seq`、`logical_version`、baseline
generation、redirect handle、descriptor owner 和 SSD target。一次 128 KiB 写形成 32 个连续
NVPage 时，这些校验及其锁操作仍随页数线性增长。

本需求用 entry 级排他所有权和 soft-update 持久化顺序保证迁移安全。正常迁移路径不再依赖
per-NVPage generation 或事后精确身份比较。

## 2. 术语

**NV redirect extent entry**：独立于 SSD-only extent tree 的持久化重定向范围记录。它同时描述
连续的文件逻辑页、NVPage slot 和对应的 SSD baseline block：

```text
[logical_start, logical_start + page_count)
    -> [nv_slot_start, nv_slot_start + page_count)
    -> [ssd_block_start, ssd_block_start + page_count)
```

一个 NV redirect extent entry 最多包含 128 个 NVPage。一个用户写请求可以生成多个 entry，但迁移器
不能根据页面连续性跨 entry 合并，也不能因迁移批次预算或索引分区边界拆分一个 entry。

当前 image v6 的 SSD-only extent tree 保持不变。NV redirect extent entry 与一个连续 SSD
baseline 子范围一一对应，但不要求与一个完整的 SSD extent-tree entry 具有相同边界；它只要求
该子范围内的 SSD blocks 连续，且不跨越 SSD extent 的物理断点。若已有 SSD baseline 或首次
分配的 SSD blocks 不连续，即使逻辑页和 NVPage slot 连续，前台也必须在 SSD 物理断点处建立
多个 redirect extent entries。每个 entry 的迁移只生成一个连续 SSD BIO。

因此，迁移不是“把 NV extent entry 转换并插入 SSD extent tree”。前台在发布 NV redirect
entry 之前已经建立稳定的 SSD baseline；rTIO 只把该 entry 的数据写入预定的连续 SSD 子范围，
设备 flush 完成后再切换权威副本。正常 completion 和回收路径不修改、分裂或重新路由 SSD
extent tree。

**迁移批次**：一次提交到块设备并共享 completion 和设备 flush 的若干 NV extent entry。
批次是 I/O 调度单位，不改变每个 entry 的独立所有权和回收边界。

**回收工作项（reclaim work item）**：completion 在 SSD 数据完成并执行设备 flush 后发布的
entry 级待处理工作。它携带运行期 entry 对象，不携带逐页 generation 数组。

## 3. 目标与非目标

目标：

1. 一个迁移对象严格对应一个持久化 NV redirect extent entry。
2. 候选选择、迁移 claim、I/O completion、权威映射切换和 NVPage 回收均以 entry 为单位。
3. 用 entry 级排他 lease 阻止重叠写入；回收开始后阻止新 NV 读者进入。
4. completion 只确认 I/O 和发布回收工作项，不读取 extent tree，不逐页校验 descriptor。
5. 回收路径先持久化把 range record 切换为 SSD 权威，再清除其 selector，最后发布空闲范围。
6. 删除数据 NVPage 的 `allocation_seq` 及迁移路径中的 per-page generation 校验。
7. 保持非重叠 entry 的前台写、迁移和回收能够并行。

非目标：

- 本需求不删除持久化 extent node、inode record、NVM allocation metadata 等元数据对象的
  generation；这些对象仍使用 `PersistentObjectRef`。
- 本需求不改变 FAST/FAIR 的树节点分裂算法。
- 本需求不实现新的 per-CPU NVPage allocator。
- 本需求不改变 SSD block-group allocator。
- 本需求不把 migration transaction 持久化到 NVDIMM。

## 4. 故障模型

系统可以在任意 NVDIMM persist、persistence fence、SSD I/O completion 或设备 flush 之后
崩溃。DRAM 中的候选索引、entry lease、迁移批次和回收工作项全部丢失；NVDIMM 内容和已经通过
设备 flush 的 SSD 内容保留。本设计不处理 NVDIMM 介质损坏，也不假设未完成设备 flush 的
SSD 写入具有持久性。

恢复过程不继续崩溃前的迁移事务。恢复器扫描有效的 `NvRedirectExtentRecord`，重建
entry 级 dirty index，并重新发起迁移。

## 5. Entry 级运行期对象

前台发布一个 NV redirect extent entry 时，同时建立一个 DRAM 对象：

```rust
struct DirtyNvExtentEntry {
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    page_count: u16,
    nv_slot_start: u64,
    ssd_block_start: u64,
    state: AtomicU8,
    readers: AtomicUsize,
    waiters: WaitQueue,
}
```

`page_count` 的范围是 `1..=128`。三个起点加同一个 offset 后，分别得到文件逻辑页、NVPage
slot 和 SSD block。该对象由持久化 redirect extent record 恢复，不保存 extent leaf locator。

持久化 `NvRedirectExtentRecord` 只需要两个可恢复阶段：`NvAuthoritative` 和
`SsdAuthoritativePendingFree`。前者表示 redirect 生效；后者表示 SSD baseline 已恢复权威，但
NV slots 仍保留给尚未退出的读者。selector 清零表示整个范围已经持久化释放。迁移的
`Migrating/IoComplete/Reclaiming` 仍只存在于 DRAM。

索引存储 `Arc<DirtyNvExtentEntry>`。`Arc` 的对象身份只在 DRAM 中使用，用来防止旧 completion
或旧回收工作项访问后来复用的 NVPage slot；它不写入持久介质，也不需要整数 generation。

状态机为：

```text
Dirty -> Migrating -> IoComplete -> Reclaiming -> Reclaimed
           |               |
           +----failure----+-> Dirty
```

每个状态转换只能由持有该 `Arc` 的执行者通过 compare-exchange 完成。`Reclaimed` 是终态，旧工作
项再次执行时只返回 `AlreadyReclaimed`，不得通过 slot 编号查找新对象。

## 6. 前台 entry 维护

前台事务负责同时维护持久化 redirect extent entry 和 DRAM dirty-entry 索引：

1. 分配连续 NVPage 范围并初始化数据和最小恢复记录。
2. 持久化 NVPage 内容和恢复记录。
3. 发布一个包含 `(logical_start, page_count, nv_slot_start, ssd_block_start)` 的
   `NvRedirectExtentRecord`。
4. 在 DRAM 索引中发布对应的 `Arc<DirtyNvExtentEntry>`。

redirect entry 分裂时，前台在同一 redirect 修改临界区内撤销旧索引项，并为持久化后的左、中、
右 entry 分别建立索引项。只有持久化 redirect records 已经合并时，DRAM 索引才允许合并。

候选索引不得再调用 `build_dirty_migration_extents()` 从逐页 redirect 推导边界。索引分区使用
entry 起点选择所有者，但一个索引项始终完整保存整个 entry；entry 跨越逐页 redirect 分区时
也不能被切开。

## 7. Entry 级访问排他

读取 NVPage 时，查询路径先取得 `DirtyNvExtentEntry` 的读引用，再计算
`nv_slot_start + logical_offset`。取得读引用和增加 `readers` 必须发生在同一个 entry 状态检查中：

- `Dirty`、`Migrating`、`IoComplete`：允许从稳定的 NVPage 进入；
- `Reclaiming`：R1 之前拒绝新读者并等待已有 readers；R1 之后新读者重新查询并走 SSD；
- `Reclaimed`：重新查询 SSD baseline。

迁移线程执行 `Dirty -> Migrating` 后禁止重叠前台写，但 NVPage 内容保持不变，所以读取可以与
SSD I/O 并行。回收线程执行 `IoComplete -> Reclaiming` 后才禁止新读者并等待已有 `readers`
归零。这个过程每个 entry 执行一次，不为每个 NVPage 获取独立迁移锁。

前台写在进入 inode/extent 修改流程前检查 entry 状态。与 `Migrating` 或 `IoComplete` entry
重叠时释放已取得的写路径资源，等待该 entry 完成；非重叠 entry 不受影响。

## 8. rTIO 迁移流程

### 8.1 候选选择

候选队列直接返回完整 `Arc<DirtyNvExtentEntry>`。一次迁移 round 可以认领多个 entry，累计页数
达到批次软上限后停止。因为单个 entry 最大为 128 页，最后一个完整 entry可以使批次略微超过
软上限；不得从 entry 中间切分。

### 8.2 Claim

对每个候选项执行一次 `Dirty -> Migrating`：

1. 从 dirty-entry 索引中移除或标记为 claimed。
2. 禁止重叠前台写；已有和新的 reader 仍可读取稳定 NVPage。
3. 从已 claim 的 entry 对象取得不可变六元组：
   `(fs_id, inode, logical_start, page_count, nv_slot_start, ssd_block_start)`。
4. 按 `nv_slot_start + offset` 构造 DMA segment，并提交到 `ssd_block_start + offset`。

步骤 3 不重新读取持久化 record，不遍历 inode 到 leaf 的路径，不展开 descriptor，也不检查每页
`allocation_seq`、`logical_version` 或 redirect handle。entry lease 保证运行期对象在迁移和回收
结束前不被替换。

### 8.3 I/O 提交与 completion

迁移线程根据 entry 的连续 SSD target 建立一个多 segment BIO。多个 entry 可以共享一个
`MigrationBatch` 和一次设备 flush。

completion 只执行：

1. 汇总 BIO 状态；
2. 批次全部成功后执行一次设备 flush；
3. 将每个 entry 从 `Migrating` 转换为 `IoComplete`；
4. 为每个 entry 发布一个回收工作项并唤醒容量等待者。

completion 不检查 descriptor、baseline generation、redirect handle 或 `allocation_seq`，也不访问
inode root、extent leaf 和 SSD allocation metadata。

## 9. 前台帮助回收

需要 NVPage 容量的前台线程可以认领一个 `IoComplete` 工作项并执行
`IoComplete -> Reclaiming`。后台回收线程、`fsync` 和 unmount 使用相同接口保证进展。

回收顺序为：

| 阶段 | 操作 | 崩溃后的有效状态 |
| --- | --- | --- |
| R0 | entry 为 `IoComplete`，SSD 已完成设备 flush | NV redirect 仍权威 |
| R1 | 将 record 持久化为 `SsdAuthoritativePendingFree`，使对应 SSD baseline 重新可见 | SSD 映射权威；NVPage 仍已分配 |
| R2 | 已有 NV readers 退出后，清除该 entry 的一个 record selector 并执行 fence | SSD 映射权威；完整 NVPage 范围已持久化释放 |
| R3 | 在 entry lease 下批量更新 DRAM data-slot records 和 free-range index | SSD 映射权威；连续范围可被 allocator 使用 |
| R4 | 发布容量、唤醒等待者，状态改为 `Reclaimed` | 可安全重新分配 |

R1 必须先于 R2 持久化。任何 reader 仍持有 entry 读引用时不得进入 R2。R2 完成前不得把 slot
暴露给 allocator。当前实现没有独立的持久化 NVPage 分配位图；range record selector 是否有效
就是整个范围的持久分配状态。恢复遇到 `SsdAuthoritativePendingFree` 时直接完成 selector 清除和
范围回收，不重新迁移。新分配先初始化页面和恢复记录，最后才发布新的 redirect entry。

回收使用 entry 级 API：

```rust
fn reclaim_nv_extent_entry(
    pool: &Arc<Rnvpc>,
    entry: &Arc<DirtyNvExtentEntry>,
) -> Result<ReclaimOutcome>;
```

正常路径不再调用 `clear_redirects_exact(&[(PageKey, DescriptorHandle)])`。替代接口清除一个 entry
selector、批量更新运行期 range record 和 redirect 索引，并归还一个连续范围：

```rust
fn clear_claimed_nv_extent_entry(
    &self,
    entry: &DirtyNvExtentEntry,
) -> Result<Range<NvPageId>>;
```

该接口只接受状态为 `Reclaiming` 的 entry lease，不接收逐页 expected handle。为使运行期状态更新
也按 entry 数而不是页数获取锁，data-slot runtime table 需要新增 range-owned 表示；仅在现有
per-slot `ObjectTable` 外增加 entry 状态不足以实现该目标。

## 10. 数据 NVPage 身份

本需求用 entry 级 `NvRedirectExtentRecord` 替代逐页 descriptor 身份。该记录只保留恢复扫描重建
redirect range 所需的信息：格式版本、filesystem、inode、逻辑起点、页数、NV slot 起点和 SSD
block 起点。运行期 object record、redirect handle、迁移参数和恢复选择逻辑不再保存
`allocation_seq`。

数据页引用改为：

```rust
struct NvDataSlotRef {
    slot_plus_one: u64,
}
```

`PersistentObjectRef { slot_plus_one, allocation_seq }` 暂时只用于持久化元数据对象。读取数据页时
使用 entry 读引用和预期 `PageKey` 检查 owner；回收安全性来自 R1 到 R4 的持久化顺序及 entry
排他状态，而不是 slot generation。

数据 NVPage 的持久化状态只服务恢复扫描。迁移的 `Migrating/IoComplete/Reclaiming` 状态位于
DRAM，掉电后不恢复。数据 NVPage 的逐页 `NvDescriptor` 在本轮由 range record 取代；元数据
descriptor 的拆分和精简属于后续需求。

## 11. 正确性不变量

1. 每个 dirty-entry 索引项与一个持久化 `NvRedirectExtentRecord` 一一对应。
2. 每个 redirect entry 的逻辑页、NVPage slot 和 SSD block 三个范围长度相同且分别连续。
3. 一个 entry 同时最多有一个 migration/reclaim owner。
4. `Migrating` 之后不能发生重叠写入；`Reclaiming` 之后不能产生新的 NV reader。
5. SSD 未完成设备 flush 时，NV redirect 始终是权威副本。
6. record 未持久化转换为 `SsdAuthoritativePendingFree` 时，其 selector 不能被清除。
7. range record selector 未持久化清除时，slot 不能进入 free pool。
8. allocator 未完成新页面初始化时，新的 redirect entry 不能引用该 slot。
9. 非重叠 entry 不共享 inode 级迁移锁。
10. 迁移、completion 和回收正常路径的锁获取次数按 entry 数增长，不按 NVPage 数增长。

## 12. 验收标准

- 一个 128 KiB 且 SSD target 连续的写形成一个 `page_count=32` 的 NV redirect extent entry、一个迁移 claim 和一个回收
  工作项。
- SSD target 出现物理断点时，前台在该断点建立两个 redirect entries；每个 entry 各自对应一个
  连续 SSD baseline 范围。
- 两个相邻但独立的 extent entry 不被候选索引自动合并。
- 一个跨 redirect 索引分区边界的 extent entry 不被拆分。
- completion 不调用 `current_page_mapping()`、`validate_baseline_token()`、
  `clear_redirects_exact()` 或 per-page descriptor read。
- 回收一个 N 页 entry 只执行一次 entry 状态 claim、一次 authority 转换、一个 range-record selector clear
  批次、一次连续范围归还和一次容量通知。
- 在 R1、R2、R3 和 R4 后模拟崩溃，恢复后读取只能得到完整 NV 数据或完整 SSD 数据，不引用
  已释放或重新分配的 NVPage。
- 旧 completion 或重复回收工作项不能修改已经重新分配的 slot。
- 4 KiB 随机写不出现正确性或吞吐回退；128 KiB 顺序写的 candidate、completion compare 和
  reclaim 锁操作次数从按页增长降为按 entry 增长。
