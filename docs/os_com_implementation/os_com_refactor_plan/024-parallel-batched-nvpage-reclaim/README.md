# 024：并行批量 NVPage 回收

## 1. 问题

当前 rNVPCe 已按 rTIO worker 并行提交迁移 I/O，但所有完成的 redirect extent entry 都进入同一个
`nv_reclaim_queue`。前台容量等待者和单个后台 helper 竞争该队列，随后逐 entry 执行权威状态发布、
redirect 索引删除、`data_ranges` 删除、slot 释放和容量唤醒。32 线程下，大量完成页停留在
reclaim-pending 状态，I/O 并非主要瓶颈，回收路径的共享锁、逐 entry 持久化和逐 entry 唤醒才是
容量不能及时转为可用状态的直接原因。

## 2. 目标

- 完成项由提交它的 rTIO worker 发布到该 worker 的回收队列，并由同一 worker 批量处理。
- 前台线程不再争抢一个全局回收队列；容量不足时只负责唤醒 worker 并等待批量释放结果。
- `data_ranges` 按 NV allocation domain 分区，互不重叠的 domain 可并行更新。
- 一批 entry 共享持久化屏障、allocation-domain 锁、容量记账和唤醒，但每个 entry 仍可独立恢复。
- 不恢复 per-page `allocation_seq`/generation 校验，不改变 on-NVM redirect entry 格式。

## 3. 调度

每个 rTIO worker 拥有一个 `ReclaimReadyQueue<ReclaimWorkItem>`。durability completion 根据
`owner_worker_id` 将整个 batch 的 entry 发布到对应队列。worker 每轮依次处理 completion、回收、
候选选择和 I/O 提交；其回收队列非空时无需等待新的前台唤醒。

容量等待者不直接 claim 任意 worker 的 entry。它广播容量压力，让已有 worker 并行排空各自队列，
并睡眠等待实际容量释放。sync、unmount 和失败清理显式遍历全部 worker 队列并等待
`nv_reclaim_pending == 0`。

第一阶段不实现跨 worker work stealing。若负载测量证明长期偏斜，再增加只转移完整 work item 的
有界窃取，不能把一个 redirect extent entry 拆给多个执行者。

## 4. 批量回收协议

一次 worker 回收批次最多处理 `MIGRATION_BATCH_PAGES` 页：

1. 从本地队列 claim 多个完整 entry，并等待各 entry 的读者退出；
2. 为所有 entry 构造 `SsdAuthoritativePendingFree` 记录，写入后执行一次持久化屏障；
3. 清除全部 entry 的 head selector，写入后执行一次持久化屏障；
4. 按 inode 批量删除 redirect entry；
5. 按 allocation domain 批量删除 `data_ranges` owner，并在每个 domain 锁下释放本批范围；
6. 聚合释放 data capacity，每个批次至多唤醒一次容量等待者队列；
7. 完成每个 lease，并统一更新 `nv_reclaim_pending` 和迁移进度。

步骤 2 完成而步骤 3 未完成时，NVM 仍保存可解释的 pending-free entry；步骤 3 完成后 SSD 为权威，
恢复扫描可以完成尚未执行的运行时清理。因此批量仅合并屏障和运行时临界区，不要求整批原子提交。

旧 visibility generation 为 0 的 legacy entry 保留原兼容路径，不与 native batch 混合。

## 5. `data_ranges` 分区

`ObjectTable` 使用与 `NvSlotAllocator` 相同的 allocation-domain 几何建立多个 owner map。一个
redirect extent entry 的物理 NV 范围已经保证不跨 domain，因此发布、查找和删除只锁一个 domain。
批量删除先按 domain 分组，再一次持锁处理该组范围。owner map 仍是 DRAM 正确性索引，掉电恢复从
持久化 descriptor/redirect 图重建。

## 6. 验收

- ktest 覆盖 worker 队列隔离、总 ready 页统计、owner worker 路由、批量释放和重复/失效 owner 拒绝。
- crash/fault tests 覆盖 pending-free 发布前后和 selector 清除前后。
- `cargo fmt --all -- --check`、`cargo osdk check --ktests --package aster-kernel` 通过。
- `/dev/nvme1n1p2` 上使用 2 GiB 低碎片文件、64 MiB data limit、4 个 rTIO worker，运行 4 KiB
  random overwrite 的 4/16/24/32 线程重复实验。
- 32 线程平均带宽不低于 4 线程；4/16 线程平均带宽回退不超过 5%。若未达标，报告必须给出
  per-worker ready/reclaimed pages、平均批次页数、容量等待和持久化/唤醒次数，不能把波动写成提升。

## 7. 实现结果（2026-11-16）

已实现：

- durability completion 按 `owner_worker_id` 投递到四个 worker-local reclaim queue；前台容量等待者不再
  竞争回收队列，sync/unmount 遍历全部队列排空；
- native entry 每批最多 128 个 entry，共享 pending-free 发布、selector 清除和 descriptor 清理的
  持久化屏障；legacy entry 保留逐 entry 兼容路径；
- redirect index 按 inode 成组删除，`data_ranges` 与 slot free 按 allocation domain 成组处理；容量只做
  一次聚合释放和唤醒；
- `data_ranges` 从单把全局锁改为与 NV slot allocator 相同的 allocation-domain 分区锁。

真实盘最终单轮 4/16/24/32 线程带宽分别为 577.88、537.39、513.08、434.73 MiB/s。相对改动前
24/32 线程的 258.82/268.20 MiB/s，分别提升 98.2%/62.1%，20 线程后的回收断崖已消除；但 32 线程
仍只有本轮 4 线程的 75.2%，未达到“32 线程不低于 4 线程”的完整目标。插桩表明回收批次已经稳定在
约 127 页，剩余差距主要对应容量等待和前台 persist/post-commit 随并发增加，而不是回收队列再次退化
为逐页处理。完整数据见 `docs/experiments/reports/RNVPCE-PARALLEL-BATCHED-RECLAIM-2026-11-16.md`。

