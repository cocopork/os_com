# EXT4 Refactor References

This index is a bounded map of proposed EXT4 work. Current implementation and validation facts are maintained only in [`../STATUS.md`](../STATUS.md).

| ID | Mechanism | Current status | Code evidence | Validation | Remaining work | Detail |
| --- | --- | --- | --- | --- | --- | --- |
| 001 | PageCache dirty-page index | Partial | Historical source study; no committed baseline named in `STATUS.md` | Runtime validation unverified | Confirm committed implementation and state-machine tests | [001](001-pagecache-dirty-index/README.md) |
| 002 | Background writeback | Partial | EXT4 writeback is in the committed baseline | Runtime validation unverified | Confirm daemon/throttle behavior and completion tests | [002](002-background-writeback/README.md) |
| 003 | Group-level allocation locks + in-memory free-space index + capacity reservations | Implemented / measured; no stable speedup | Rebuild 2026-10-17; independent group gates, hierarchical index and unified capacity ledger; no PA | 593 ktests; 12 fio samples; 4GiB remount CRC32C; 15 clean e2fsck runs | Investigate group/journal contention and sequential regression; complete supplemental fault matrix | [003](003-buddy-cache-allocator/README.md) |
| 004 | Extent flatten elimination | Partial | Existing source retains rare rebuild paths | Not run | Bound rare range-edit paths | [004](004-extent-flatten-elimination/README.md) |
| 005 | virtio-blk segment merge | Planned | Filesystem BIO merging is separate from virtio descriptors | Not run | Implement and measure driver-side merging | [005](005-virtio-blk-segment-merge/README.md) |
| 006 | virtio-blk multiqueue | Planned | Existing study identifies one-queue fallback | Not run | Evaluate MQ independently | [006](006-virtio-multiqueue/README.md) |
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
