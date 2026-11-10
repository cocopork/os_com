# EXT4 Refactor References

This index is a bounded map of proposed EXT4 work. Current implementation and validation facts are maintained only in [`../STATUS.md`](../STATUS.md).

| ID | Mechanism | Current status | Code evidence | Validation | Remaining work | Detail |
| --- | --- | --- | --- | --- | --- | --- |
| 001 | PageCache dirty-page index | Partial | Historical source study; no committed baseline named in `STATUS.md` | Runtime validation unverified | Confirm committed implementation and state-machine tests | [001](001-pagecache-dirty-index/README.md) |
| 002 | Background writeback | Partial | EXT4 writeback is in the committed baseline | Runtime validation unverified | Confirm daemon/throttle behavior and completion tests | [002](002-background-writeback/README.md) |
| 003 | Group-level allocation locks + in-memory free-space index + capacity reservations | Implemented / measured; no stable speedup | Rebuild 2026-10-17; independent group gates, hierarchical index and unified capacity ledger; no PA | 593 ktests; 12 fio samples; 4GiB remount CRC32C; 15 clean e2fsck runs | Investigate group/journal contention and sequential regression; complete supplemental fault matrix | [003](003-buddy-cache-allocator/README.md) |
| 004 | Extent flatten elimination | Partial | Existing source retains rare rebuild paths | Not run | Bound rare range-edit paths | [004](004-extent-flatten-elimination/README.md) |
| 005 | virtio-blk segment merge | Planned | Filesystem BIO merging is separate from virtio descriptors | Not run | Implement and measure driver-side merging | [005](005-virtio-blk-segment-merge/README.md) |
| 006 | virtio-blk multiqueue | Superseded | Historical standalone placeholder | Not run | Replaced by end-to-end rTIO/virtio/host-NVMe plan 021 | [006](006-virtio-multiqueue/README.md) |
| 007 | Delalloc completion | Partial | Delalloc is in the committed EXT4 baseline | Runtime validation unverified | Resolve remaining regressions and run targeted recovery checks | [007](007-delalloc-completion/README.md) |
| 008 | Nojournal metadata writeback | Planned | Diagnostic source only | No accepted experiment | Design batching without changing durability rules | [008](008-nojournal-delalloc-meta-writeback/README.md) |
| 009 | fsync device round trips | Planned | Diagnostic source only | No accepted experiment | Confirm causes on committed baseline and test ordering | [009](009-fsync-device-roundtrips/README.md) |
| 010 | NVPool port | Superseded | Replaced by committed rNVPC/rNVPCe/rTIO baseline | Runtime validation unverified | Use AchillesFS references for follow-on work | [010](010-nvpool-port/README.md) |
| 011 | Nojournal versus journal writeback study | Partial | Historical diagnostic study | No accepted experiment | Reproduce with retained evidence before using conclusions | [011](011-nojournal-vs-journal-writeback/README.md) |

Status values are intentionally limited to `Complete`, `Partial`, `Planned`, `Superseded`, and `Blocked`. They do not promote historical plans or benchmark prose into completed claims.

## 2026-10-19 新增设计

- [012：Orderless SSD 写回与批量迁移回收](012-orderless-migration/README.md)：rNVPCe rTIO默认采用data与分配metadata的DRAM批次、完成后单次SSD flush、NVM权威发布及pending-target挂载保护；旧的两屏障ordered migration路径已删除。真实NVMe三轮显示128KiB顺序写16线程含drain提升12.1%，4KiB随机写1线程提升2.7%、16线程回退11.2%。不可变native metadata快照、完整注册恢复与孤儿回收仍未实现；见[实现计划](012-orderless-migration/IMPLEMENTATION_PLAN.md)和[测试报告](../../experiments/reports/ORDERLESS-MIGRATION-2026-10-19.md)。

## 2026-10-20 新增设计

- [014：FAST/FAIR 风格的 NVM extent 原地更新](014-fast-fair-inplace-extents/README.md)：Planned，当前方向。保留混合 entry，移除 entry 的 allocation_seq，8B target 原子发布、字段级更新、安全 slot 回收和完整 NVM 路径；替代整节点 repair image。见[实现计划](014-fast-fair-inplace-extents/IMPLEMENTATION_PLAN.md)。区间 FAST 协议须先通过状态模型验证，尚未实施。

- [013：原生 EXT4 extent 与独立持久化重定向表](013-extent-redirection/README.md)：Superseded（2026-10-20）。用户改为保留 ExtentRecordEntry，参考 FAST/FAIR 以字段级原地持久化替代整节点 repair image；不实施重定向表。原文和任务仅作历史记录，替代协议尚待完整设计与验证。

## 2026-10-21 新增设计

- [015：连续 NV extent 与整段迁移](015-contiguous-nv-extents/README.md)：Implementing。连续 NVPage 由一个范围 entry 表示；前台批量发布描述符，completion 只在完整 extent 及连续 SSD 目标均验证通过后切换一次 target 并批量回收。dirty extent 内存索引留作后续优化。

## 2026-10-22 新增设计

- [016：Extent 级迁移 lease 与写入准入](016-extent-migration-lease/README.md)：Superseded。跨 SSD I/O 保留 leaf guard 和 inode 范围 gate 的实现已删除；当前乐观迁移只在完整 NV extent 的 claim 和 completion publication 临界区短暂持锁，I/O 期间允许同 inode 前台写并发。见[替代设计](../../superpowers/specs/2026-10-22-optimistic-extent-migration-design.md)和[验证报告](../../experiments/reports/OPTIMISTIC-EXTENT-MIGRATION-2026-10-22.md)。

- [017：NVPage 并行回收](017-nvpage-parallel-reclaim/README.md)：Runtime scheduling phase implemented，性能验收未通过。completion 在批次设备 flush 后只发布回收工作项（reclaim work item）；首次写的前台事务已预分配 SSD blocks 并发布 baseline，helper 只做精确 redirect 清除和 NVPage 批量归还，不再修改文件 extent tree。单等待者和无等待者场景由后台 helper 保证进展；`io_in_flight` 与 `reclaim_pending` 已分离。per-CPU free-run cache 与索引分区 free pool 尚未实现。
- [018：以 NV extent entry 为单位的迁移与校验消除](018-nv-extent-entry-migration/README.md)：Implemented，性能验收待执行。保持 SSD-only extent tree；一个持久化 NV redirect extent entry 同时对应连续的逻辑页、NVPage slots 和 SSD baseline blocks。entry 级 lease 取代 per-NVPage generation、redirect handle 和 completion 事后校验，回收遵循“先切换 SSD 权威、再清除 head selector、最后发布空闲范围”的 soft-update 顺序。DRAM allocator 的 per-slot free 记录更新仍待 range-owned table 优化。
- [019：rNVPCe 范围写事务](019-range-write-transactions/README.md)：Approved design，待实现。将 `PageKey` 降为范围内查询坐标，以 NV redirect extent entry 作为 Native 数据面的权威、版本、并发和生命周期单位；连续 128 KiB 写通过一次范围路由、一次连续 NV 分配和两阶段 entry 发布完成，元数据工作按范围断点数量而非 4 KiB 页数增长。配套[实现计划](../../superpowers/plans/2026-10-31-rnvpce-range-write-transactions.md)。
- [021：rTIO 与 virtio-blk 多队列](021-rtio-virtio-multiqueue/README.md)：实现完成，真实设备正确性与机制验收通过。2026-11-02 的 8 个 2 GiB 点通过 fio/sync/unmount/e2fsck；四 worker 与 virtqueue 一一绑定，QEMU 10.2.1 独立 IOThread 和 NVMe 提交点跟踪确认 hctx 0–3 工作（最大占比 25.75%–31.07%）。该版本随机写含 sync 吞吐量提高、顺序写回退，因而曾恢复单 worker 默认。后续 inode 归属 flusher 与 epoch 聚合改变了调度路径；自 2026-11-05 起，rNVPCe 默认使用 `min(4, submission_queue_count)` 个 worker，显式 `rtio_workers=N` 仍可覆盖，rNVPC WAL 保持单 worker。见[实验报告](../../experiments/reports/RTIO-VIRTIO-MULTIQUEUE-2026-11-02.md)和[实现计划](021-rtio-virtio-multiqueue/IMPLEMENTATION_PLAN.md)。
- [022：inode 归属的 rTIO flusher 与分布式完成处理](022-inode-owned-rtio-flushers/README.md)：已实现，真实 NVMe 多 inode 性能验收通过。rNVPCe 专用 completion 线程已删除；每个 inode 在活动期固定由一个 flusher claim、提交、完成和回收，BIO callback 只投递 owner 本地队列，非阻塞 durability leader 为每个封闭 epoch 执行一次设备 FLUSH。8 个 2 GiB 点均通过 fio/sync/umount/e2fsck；四 worker 的 4 KiB 随机和 128 KiB 顺序 16-job 含 sync 吞吐量分别提高 77.9% 和 14.4%。遗留问题是 4 KiB/16 job 的 epoch/FLUSH 数从 520 增至 70,790，需要后续合并 durability epoch。见[实验报告](../../experiments/reports/RTIO-INODE-OWNED-FLUSHERS-2026-11-03.md)和[实现计划](022-inode-owned-rtio-flushers/IMPLEMENTATION_PLAN.md)。
- [023：rTIO durability epoch 自适应聚合](023-rtio-durability-epoch-coalescing/README.md)：已实现并通过正确性验证，性能部分通过。四 worker 的 4 KiB/16 job epoch 从 70,790 降至 1,791，含 sync 带宽提高 3.6%；128 KiB/16 job epoch 降至 2,549，但含 sync 带宽下降 11.5%。下一步需要缩小或自适应调整聚合目标，限制 completion 等待。见[实验报告](../../experiments/reports/RTIO-DURABILITY-EPOCH-COALESCING-2026-11-03.md)。
- [Per-page descriptor redirection](../../superpowers/specs/2026-10-27-page-descriptor-redirection-design.md)：Superseded by 018。保留了“前台先建立 SSD baseline、普通 migration/completion 不修改 extent leaf”的结论；逐页持久化 NVPage descriptor、逐页 generation 和动态拼接迁移范围已由持久化 NV redirect extent entry 取代。
