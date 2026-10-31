# 017：NVPage 并行回收

日期：2026-10-22。状态：Implemented (runtime scheduling phase, performance acceptance not met, 2026-10-28)。

实现说明：后续的 per-page descriptor redirection 设计已取代本文“混合 SSD/NV extent
target”的数据格式假设。2026-10-29 起，首次写由前台批量分配 SSD blocks、发布 NVM 权威
allocation bitmap 和 SSD baseline extent；redirect 在设备 flush 前继续覆盖该 baseline。正常
completion 在批次 flush 后只发布经过 generation 校验的回收工作项（reclaim work item），helper 只清除精确
redirect 并批量归还 NVPage，不再路由或修改文件 extent tree。零或一个等待线程时由后台
helper 承担回收，避免单 writer 把回收串入 write 调用；sync/unmount 同时 drain
`io_in_flight` 和 `reclaim_pending`。per-CPU free-run cache 和分片 free pool 尚未实现，仍作为
后续 allocator 优化。实测与剩余瓶颈见
[`NVPAGE-PARALLEL-RECLAIM-2026-10-28.md`](../../../experiments/reports/NVPAGE-PARALLEL-RECLAIM-2026-10-28.md)。
当前实现通过正确性回归，但只有 128 KiB/16 线程吞吐改善，单线程和随机写性能验收未通过。

## 1. 问题与目标

当前 rTIO 由单个 completion 上下文串行完成整条回收链路：处理已经完成的 SSD I/O、
验证并发布 extent 的 SSD target、清除 NVPage 描述符、归还全局 free list，最后才唤醒
等待容量的前台线程。即使队列前部的 extent 已经满足回收条件，前台仍可能等待整个
completion 队列处理完毕。多个被唤醒的写线程随后又竞争同一个全局 allocation lock。

本需求将 I/O 完成确认与权威引用切换解耦：

1. completion 在批次内所有 data 和 EXT4 metadata 请求完成并执行一次设备 flush 后，
   只把完整 NV extent 标记为 `SsdDurable`，加入可帮助回收队列；
2. 需要 NVPage 的前台线程在进入自身 inode/extent 修改流程之前，从该队列认领一个
   `SsdDurable` extent；
3. 前台 helper 验证预发布 baseline 与工作项中的 target、清除精确描述符并归还槽位；
4. 不同 helper 可以并行处理不同回收工作项；正常路径没有 inode extent publication 临界区；
5. 回收得到的连续 NVPage run 优先放入当前 CPU 的 free cache，减少前台分配对全局锁的竞争。

设计目标不是在 SSD 吞吐不足时避免所有写入阻塞，而是消除“SSD 已持久化且已有可释放
extent，前台却等待单个 completion 线程串行发布整个队列”的软件瓶颈。

## 2. 关键定义

`SsdDurable` 不等于 `Free`，也不能让 NVPage 立即复用。此状态只表示：

- 该迁移批次的 SSD data 和分配 metadata 请求已经完成；
- completion 已在整个批次上执行设备 flush；
- descriptor 中的 SSD target 仍由该 NVPage generation 持有；
- NVM extent tree 中已有前台预发布的 SSD baseline，但 descriptor redirect 仍覆盖
  baseline，因而 NVPage 仍是文件数据的权威副本；
- 页 pin 和 generation 身份仍然有效。

只有 helper 验证前台已发布的 SSD target，并持久化清除 NVPage descriptors 后，槽位才能
进入 free pool。对外不能把 `SsdDurable` 计入 `free_pages`，只能单独计入
`ssd_durable_pages` 或 `reclaim_ready_pages`。

并行回收单位是完整 NV extent，而不是单个 4 KiB NVPage。一个连续 128 KiB NV extent
执行一次 baseline 校验、一次批量 descriptor clear 和一次连续 run 归还；不在回收路径内
再执行 extent target 切换。

## 3. 状态机

```text
NvDirty
   |
   | rTIO claim：固定完整 extent 和成员 generation
   v
InFlight
   |
   | data/metadata I/O completion + batch device flush
   v
SsdDurable
   |
   | helper CAS/claim
   v
ReclaimingPreCommit
   |
   | 验证前台已发布的 SSD baseline 与精确 descriptor generation
   v
SsdPublishedCleanup
   |
   | 幂等释放 reservation credit，批量清除 descriptors 并持久化
   v
Free
```

`SsdDurable -> ReclaimingPreCommit` 是 DRAM 原子认领状态。同一 extent 同时只能由一个
helper 处理。精确 descriptor selector 持久化清除是权威副本从 NVPage 切换到已经持久化的
SSD baseline 的不可逆提交点，随后回收工作项必须转入 `SsdPublishedCleanup`；此后任何错误或
owner 退出都只能把该工作项放进 cleanup queue，绝不能退回 `SsdDurable/NvDirty`。新的 helper
接手时若精确 generation 的 selector 已清除，应把权威切换视为已经成功并继续幂等清理。

提交点之前的临时锁冲突可以把任务重新放回 `SsdDurable` 队列。普通并发导致的 descriptor
generation 替换或删除，与 descriptor/target 自相矛盾必须区分：前者进入重查、取消或
orphan cleanup，后者才进入 migration failure/recovery 路径。

`SsdDurable` 第一版只作为运行期状态，不增加新的 NVDIMM 持久化阶段。该状态掉电后丢失，
恢复仍以 NVM extent 为权威，重新提交 data/metadata 写和设备 flush。这样不需要持久化完整
migration transaction，也不会把“BIO 完成但尚未 flush”的 SSD 内容误认为权威副本。

## 4. completion 的职责

completion 保留以下工作：

1. 收集一个 migration batch 的 data 和 metadata I/O 结果；
2. 仅当整个批次的相关请求均成功时，发送并等待一次设备 flush；
3. 以 release 顺序把各完整 extent 的回收工作项从 `InFlight` 发布为 `SsdDurable`；
4. 将工作项加入分区 reclaim-ready queue；
5. 唤醒容量等待者，使其检查“已有 free run”或“已有可帮助回收任务”。

completion 不再遍历 extent tree，不发布 leaf target，不清除 descriptor，也不直接把 NVPage
加入 free pool。它可以释放 BIO 和临时 I/O 资源，但必须把 extent 身份、成员 page pins 和
SSD target 所有权转移给回收工作项。

completion 每产生一批 `SsdDurable` 回收工作项就发布并唤醒，不得先取走整个 completion queue、
处理完所有批次后才统一通知。

## 5. 回收工作项

每个回收工作项至少携带：

- filesystem 与 inode 身份；
- extent 的逻辑起点和长度；
- 预期 NV base slot；
- 每个成员的 allocation generation；
- 连续 SSD target base 及持久化 reservation 身份；
- 全部成员的 eviction pins；
- `InFlight/SsdDurable/ReclaimingPreCommit/SsdPublishedCleanup` DRAM 状态。

回收工作项必须携带完成 descriptor 校验与清理所需的 filesystem/inode 稳定身份，不能依赖原 BIO
或原 completion stack frame。ready/cleanup queue 由 `Ext4` 拥有，因此工作项不得反向持有
`Arc<Ext4>` 或形成 inode-to-filesystem 强引用环；它保存 `Weak<Ext4>`、稳定的 fs/inode ID 和
pool/page pins。sync/unmount 在销毁 filesystem 前主动 drain ready、claimed 和 cleanup 工作项，
所以正常 helper 执行期间弱引用必须能够升级。

进度计数分成 `io_in_flight` 与 `reclaim_pending`。completion 发布回收工作项后只减少前者并增加
后者；baseline 校验、credit release、descriptor clear 和 free-run 归还全部结束后才减少
`reclaim_pending`。`fsync`、export 和 unmount 必须主动运行同一个 helper 并等待两个计数都为
零，不能依赖未来的前台写入提供进展。

## 6. 前台帮助回收

NVPage 分配准入改为以下循环：

```text
尝试从本 CPU free-run cache 预留所需页
    -> 成功：进入正常 write mutation
    -> 失败：尝试从共享 free shard 批量 refill
    -> 仍失败：认领一个 reclaim-ready extent 并帮助回收
    -> 回收成功：把 run 放入本 CPU cache，重新预留
    -> 没有可认领任务：请求 migration 并睡眠
```

helper 必须在取得本次用户写入的 inode lock、publication guard、leaf guard 之前运行。它不能
持有写入 inode A 的锁，再去回收任意 inode B 的 extent，否则会产生跨 inode 和 leaf 的锁序
环。若写计划已经进入 extent mutation 后才发现容量不足，必须释放写路径状态，再返回准入
循环帮助回收或等待。

一次帮助操作以完整 extent 为边界。线程只回收到足以满足当前 reservation，或达到一个有界
help budget；不能因为全局 ready queue 很长而在一次 write 中无限回收。后台 reclaim worker
调用同一个 helper，用于空闲期、`fsync`、卸载和没有新写线程时的进度保证。

## 7. helper 的发布与回收协议

helper 认领回收工作项后按以下顺序执行：

1. 用工作项携带的 baseline generation 和 target 校验前台已发布的 SSD mapping；
2. 验证所有 pinned generation、descriptor SSD target 和连续范围与工作项一致；
3. 批量清除该 extent 的 NVPage descriptors，flush 后共享 fence；
4. 解除 pins，把连续 slots 作为一个 free run 归还当前 CPU cache；
5. 增加可用容量并按实际释放页数唤醒等待者。

正常路径不得重新定位 inode root/leaf，也不得执行 FAST extent target store。旧版恢复镜像中
缺少预分配 target 的 descriptor 可以进入兼容性结构发布路径，但新前台写不得产生这种状态。

descriptor selector 清除是权威副本切换点。步骤 3 之前任何失败都不能复用 NVPage；步骤 3
之后即使 helper 退出，SSD 已是权威副本，恢复或其他清理路径可以完成 slot 归还。reservation credit
和 slot 归还都必须是 generation 检查保护的一次性操作。credit helper 必须把“本次释放”和
“此前已经释放”都视为成功结果，并分别避免重复更新 EXT4 运行时 block credit；现有把
already-released 当作错误的接口需要改为显式 `Released/AlreadyReleased` 结果。

回收工作项记录每个 SSD block 是否由本次 migration 新分配，以及 persistent reservation 的身份。
只有第一次把 descriptor credit 状态持久化为 released 的 owner 才归还对应 EXT4 运行时容量；
若运行时额度归还后掉电，恢复从 forward graph 和持久化 ledger 重建计数，而不是再次执行
运行期增量。

不同 helper 只需在 descriptor batch 和 free pool 层面协调，正常路径不取 inode publication
guard 或 leaf guard，因此同一 inode 中不同 dirty extents 也可并行清理。与 FAIR split 的
协调已由前台 baseline publication 完成，helper 不再参与树结构更新。

任何 helper 都不得使用 filesystem 全局 completion lock 或 inode 范围 gate。持久化和
descriptor 批处理不得在 free-pool spinlock 内执行。

## 8. free pool 组织

per-CPU cache 解决的是分配和归还锁竞争，本身不替代上述持久化协议。建议采用两层结构：

1. 每 CPU 保存少量连续 free runs，满足 4 KiB 和 128 KiB 的常见 reservation；
2. 全局层按 NVDIMM region 分片，保存溢出和跨 CPU 可窃取的 free runs；
3. 本地不足时从一个 shard 批量 refill，而不是逐页获取全局锁；
4. 本地超过上限时批量 drain 到所属 shard，防止空闲容量被单个 CPU 长期囤积；
5. 全局可用页和 data-limit reservation 使用原子计数，分片锁只保护 run 容器。

free-run cache 持有的是已经完成 descriptor clear、持久化层可证明为 free 的 slot 所有权，
不是已经分配的 `Staged` page。slot 进入本地 cache 时全局 `free_pages` 增加；线程把 run 原子
转成 reservation 时减少 `free_pages`、增加 reserved/data credit，不能在 refill 或 CPU 间
steal 时重复记账。reservation 未消费即退出时把同一 run 和 credit 原路归还。

allocation generation 改由全局 `AtomicU64` 以 run 为单位预取连续序号区间，或在 shard refill
时批量领取不重叠区间；每个 slot 执行 `Free -> Staged` 时消耗一个唯一、非零 generation。
per-CPU cache 的访问必须禁止抢占或使用显式 CPU-local guard；线程迁移后不能同时从两个 CPU
cache 消费同一个 run。shared shard 的 remove/refill/steal 在 shard lock 下原子移动整个 run。

连续 extent 必须以 run 形式流转；不能先拆成逐页 per-CPU 栈，再在每次 128 KiB write 中对
全局列表排序寻找连续槽。碎片化时可以退化为多个 maximal runs，但每个 run 都是独立的
extent entry 和迁移回收工作项。

## 9. 等待与唤醒

容量等待条件从单一的 `free_pages >= requested` 改为：

```text
有足够 free run
    OR 存在可认领的 SsdDurable 回收工作项
    OR migration/shutdown 失败
```

被唤醒的线程先原子认领回收工作项，认领失败者重新检查本地/共享 free pool，不应全部竞争同一个
allocation lock。唤醒数量应与新发布工作项数或已释放容量相匹配，避免无条件 `wake_all()`
造成惊群。等待期间不得持有 inode、extent、publication 或 free-pool 锁。

## 10. 与乐观迁移及 FAST/FAIR 的关系

本需求建立在“乐观迁移 + completion 事后验证”以及 FAST/FAIR 字段级持久化之上：

- migration I/O 期间不持有 inode publication guard、leaf guard 或 inode 范围 gate；
- `InFlight` generation 保持不可原地修改，重叠写在锁外等待后重试；
- 非重叠写可以与 SSD I/O 和其他 helper 并行；
- helper 用回收工作项中的 PageKey、generation 和 target 批量验证 descriptor；
- 普通 NV-to-SSD 切换通过批量清除 descriptor selector 完成，不读写 extent leaf；
- FAIR split 只由前台结构变更路径处理，不会使 reclaim helper 重新定位 leaf。

reverse index 可以作为候选定位 hint 保留，但 helper 的正确性依赖 descriptor 的精确 PageKey、
extent 身份和 generation。

若 truncate、unlink、hole punch 或合法覆盖已删除/替换该 descriptor generation，回收工作项在
提交点前取消 claim，仍可达的原 NV generation 整体退回 `NvDirty` 并保留 descriptor 持有的
SSD target reservation 供下次迁移复用；已经不可达的 generation 进入 orphan cleanup。只有
generation 对不上、
同一 target 被不同所有者声明等无法由正常并发解释的状态才使文件系统进入失败状态。

## 11. 掉电状态

| 掉电点 | 权威副本 | 恢复动作 |
| --- | --- | --- |
| SSD I/O 或 batch flush 前 | NVM | 忽略 SSD 中间状态，重新迁移 |
| flush 后、descriptor clear 前 | NVM | `SsdDurable` DRAM 状态丢失，根据 redirect 重新迁移并复用/核对持久化 target reservation |
| descriptor clear 后、runtime free-pool 更新前 | SSD | 扫描未被 forward edge 引用的槽位，恢复为空闲 |
| free-pool 更新后 | SSD | 正常恢复 |

任何路径都不得仅凭 per-CPU free cache 决定槽位可用。恢复以持久化 descriptor 和 forward tree
为准，per-CPU 和分片 free pool 均由恢复扫描重建。

## 12. 实施边界

第一阶段只改变运行期调度和 free-pool 组织，不改变 on-NVM extent entry 格式。需要修改的
主要边界包括：

- completion queue：输出 `SsdDurable` 回收工作项；
- migration batch：把 pins 和 extent 身份转移给回收工作项；
- write admission：在持有 inode/extent 锁前执行 bounded helping；
- baseline publication：由前台首次写在同一 write transaction 中完成，迁移与回收不再修改树；
- allocator：增加 per-CPU free-run cache、分片共享 run pool 和原子 reservation accounting；
- lifecycle：分别维护 `io_in_flight` 和 `reclaim_pending`，sync/unmount 主动 drain 两阶段；
- recovery：继续从持久化 forward graph 和 descriptors 重建容量，不信任 DRAM cache。

第一阶段不处理 SSD target 碎片化下的持久化 NV extent split；该问题仍由独立 FAST/FAIR
split 协议解决。

## 13. 验收

- completion 完成 flush 后不读取或修改 extent leaf，只发布 `SsdDurable` 回收工作项。
- 一个 32 页连续 NV extent 由一个 helper 执行一次 baseline/descriptor 批量验证和一次
  descriptor 批量清除，extent tree 读写计数为零。
- 16 个容量等待线程可以认领不同回收工作项；同 inode 的不同 dirty extents 也不因 inode
  publication guard 串行，descriptor clear 和 free-run 归还可并行。
- 同一回收工作项只能有一个 `ReclaimingPreCommit` 或 cleanup owner；模拟 ABA 后旧 helper 不能
  释放新 generation。
- descriptor clear 后在 credit release、free-run publish 各点注入错误，回收工作项
  只能进入 `SsdPublishedCleanup` 并由另一个 helper 幂等完成，不能退回 `NvDirty`。
- 前台 helper 不持有当前 write 的 inode/publication/leaf 锁，锁序测试无环。
- 每个 extent 释放后容量立即可见，不等待 completion queue 中其他 batch。
- sync、export 和 unmount 在没有前台 writer 时也能 drain `io_in_flight` 与 `reclaim_pending`，
  且回收工作项不与 `Ext4` 形成强引用环。
- 4 KiB 随机写和 128 KiB 顺序写分别在 1/16 线程下记录：容量等待时间、ready queue 等待、
  helper target publish、descriptor clear、free-pool refill、全局/分片锁等待和实际 SSD 带宽。
- fault injection 覆盖第 11 节全部掉电点，恢复后只能读到完整 NVM 或完整 SSD 版本，不出现
  悬空 target、重复 reservation credit 或复用中的 NV slot。
- 对比单 completion 回收基线，16 线程下已有 `SsdDurable` work 时不再由单线程 target publish
  吞吐限制前台；单线程场景不得因 helping 调度显著退化。
