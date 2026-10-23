# 016：Extent 级迁移 lease 与写入准入

日期：2026-10-22。状态：Superseded。

本方案的跨 I/O leaf guard 和 inode 范围 gate 已被
[`optimistic-extent-migration-design.md`](../../../superpowers/specs/2026-10-22-optimistic-extent-migration-design.md)
取代。新实现只在完整 NV extent 的 claim 和 completion publication 临界区短暂持锁，SSD
分配、BIO、设备 flush 和 completion 等待期间不保留 inode/leaf 协调状态。本文保留为历史设计，
不再描述当前代码行为。当前实现与结果见
[`OPTIMISTIC-EXTENT-MIGRATION-2026-10-22.md`](../../../experiments/reports/OPTIMISTIC-EXTENT-MIGRATION-2026-10-22.md)。

## 1. 问题

当前 rTIO 从逐页 dirty queue 取得候选，并在 candidate、claim、completion 和最终 leaf
发布阶段反复读取、校验同一个 leaf。一个 128 KiB NV extent 因而可能在真正更新一次
target 前完成多轮、每轮 32 次的相同校验。候选取得后，前台仍可修改或分裂这个 extent，
后台只能在 completion 再检查 ABA、reverse locator 和 entry index 是否变化。

前台容量等待也必须成为明确的准入条件：所需 NVPage 尚未预留时，不得进入 inode/extent
修改流程。若进入后因映射变化发现预留不足，或者目标 extent 已进入迁移，则必须释放
reservation、extent publication 和 inode 锁，等待容量或迁移状态变化后从头重试。

## 2. 迁移单位与 lease

实际 mutation guard 的粒度是 leaf node，但 rTIO 的页预算不能由一个大 leaf 中所有 NV entries
一次耗尽。因此候选身份包含 `(inode, leaf_slot, entry_index)`，每个 lease 只认领 seed entry
表示的一个 NV extent。候选 claim 在一次短 publication 临界区中完成：

1. 根据 dirty extent 身份定位 leaf entry，并取得该 leaf 的 `MetadataMutationGuard`；
2. 读取 leaf 一次，验证 seed entry 的范围、NV base slot 和所有页的 allocation sequence；
3. 为 seed extent 安装 `ExtentRangeFreeze`，并在当前实现中安装 inode 范围 migration gate；
4. 将 seed extent 的 NVPages 作为一个整体从 `NvDirty` 转为 `InFlight`；
5. 任一步失败时回滚已经取得的 page guards、range freeze 和 leaf guard，整段重新排队；
6. 成功后释放 publication，但由 `ExtentMigrationLease` 持有 leaf guard、range freeze、
   entry 快照和全部 page guards，直到 SSD I/O 与 completion 结束。

lease 阻止 seed entry 在迁移期间被原地更新、分裂或移动。completion 因此使用 claim 时已经
验证的 entry 快照，对该 NV extent 发布一次 SSD target，不再逐页读取 leaf 或重新解析 reverse
locator。由于 leaf guard 跨 I/O 保留，而前台写路径先取得 inode lock，当前 inode 范围 gate 会
阻止该 inode 的新写在 guard 存活时进入，以免形成 `inode -> leaf` 与 `leaf -> inode` 的锁序环。
这保证进展，但会限制同一 inode 上互不重叠 extent 的并发。

## 3. 前台写入准入与等待

前台采用“预检、预留、锁内复核”三步：

1. 在未持有 inode write lock、publication guard 或 leaf mutation guard 时，预检写入范围，
   计算可能新增的 NVPage 数并完成容量预留；容量不足时直接等待 rTIO 释放容量。
2. 取得 inode write lock 和 publication guard 后重新构造实际写计划。若目标范围与迁移 freeze
   重叠，立即释放 publication、inode lock 和 reservation，等待对应迁移结束后重试。
3. 若并发映射变化使实际需求超过 reservation，同样释放全部状态，补足容量后重试；不得在
   已经修改数据或 extent 后等待。

等待只发生在所有 inode/extent 锁和 reservation 已释放之后。写流程一旦开始持久化数据或
发布 extent，就不能再进入容量等待分支。现有 NVPage 若因 migration 处于 `InFlight`，应转换为
可重试的迁移冲突，而不是向应用返回短写或永久错误。

## 4. 锁顺序

正常 claim 的顺序为：

```text
inode publication -> leaf MetadataMutationGuard -> range freeze -> page eviction guards
```

claim 完成后先释放 publication，lease 才跨 SSD I/O 存活。completion 只使用 lease 内已有的
leaf guard，绝不在持有 leaf guard 时重新取得 publication。前台不得在持有 inode/publication
时等待 migration；这两条共同避免 `publication -> leaf` 与 `leaf -> publication` 环。

任何 spinlock 只用于短状态转换；NVDIMM persist、SSD I/O、设备 flush 和等待都在 spinlock
之外执行。

## 5. Soft-update 与崩溃语义

lease、freeze 和 `InFlight` 是 DRAM 协调状态，不是新的持久化权威状态：

- SSD target 发布前掉电，NVM extent 仍为权威副本；恢复后重新选择整段迁移。
- data、EXT4 metadata 和设备 flush 完成后，在 leaf guard 下原子持久化 8B SSD target；此后
  SSD extent 成为权威副本。
- target 发布后、NV descriptor 回收前掉电，只会遗留可恢复的 NV 副本；恢复按 SSD forward
  edge 回收它。
- 未完成的 DRAM lease 在重启后自然消失，不要求持久化 migration transaction。

## 6. 验收

- 一个 32 页 NV extent 只产生一个迁移 lease，claim 只读取和验证 leaf 一次。
- 同一 leaf 中不同 NV extents 使用不同候选身份，不会被 leaf 级去重丢弃。
- 任一页身份不符或繁忙时，整段 claim 失败并完整回滚，没有部分 `InFlight` 页。
- lease 存活时，重叠前台写释放 inode、publication 和 reservation 后等待；lease 释放后写入
  能从头重试成功。
- 容量不足时，前台未进入 extent mutation，未发布部分数据，也未持有 inode/extent 锁等待。
- completion 使用 lease 发布一次 target；不再有正常路径的逐页 leaf/reverse 二次校验。
- 在 claim、SSD I/O、target publish 和 descriptor clear 后模拟崩溃，读取只能得到完整 NV 或
  完整 SSD 版本。
- 重新运行 4 KiB 随机写与 128 KiB 顺序写的阶段延迟分解，重点核对 candidate、completion、
  leaf read 和 publication wait 的次数及时间。

## 7. 已知未完成项：SSD 碎片化

015 协议要求一个多页 NV extent 的 SSD targets 物理连续，才能用一次 8B target store 完成
权威切换。现有碎片化集成场景只剩隔块空闲空间，因而得到不连续 targets；直接把整个 leaf
重写一遍会重新引入 repair-image 开销，而且不能证明 cache-line 级掉电一致性，本实现拒绝用
该办法掩盖问题。

后续需要单独设计“迁移前 NV extent 持久化 split”或等价的 FAST/FAIR 状态机：先把一个 NV
extent 安全拆成与 SSD 连续 runs 对应的多个 NV entries，再各自执行 8B target 发布。在该协议
完成前，碎片化 targets 返回 `EAGAIN`，不修改权威 NVM extent，也不回收 NVPages。
