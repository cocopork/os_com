# NVPage Batch Transactions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement task by task.

**Goal:** 在128页在途窗口内批量分配、更新描述符、归还额度和回收，不改变soft-update依赖顺序。
**Architecture:** 新版本控制布局提供双描述符副本和8字节选择字；已提交数据的描述符更新不再重刷数据。lease跨越准备、发布和回收，同阶段合并fence，保留逐页身份及部分完成。
**Tech Stack:** Rust kernel、Docker、OSDK ktest、QEMU/KVM、fio。
**Spec:** README.md及用户确认的双副本/选择字设计。

## Global Constraints

- 保持nojournal、MAX_IN_FLIGHT=128、64MiB数据上限、必要SSD flush和NVM fence。
- 不引入WAL；rNVPC原有WAL回归通过；旧持久化格式显式拒绝。
- 不提交、不推送、不覆盖既有改动；使用现有recovery分支及Docker。
- 所有持久化/I/O在自旋锁外；锁顺序state→allocation→单slot。
- SSD位图孤儿所有权恢复属于009仍未完成的整体恢复边界，不宣称010解决全部文件系统掉电一致性。

## Review Focus

- 选择字部分发布、描述符内容部分写入、重复恢复及旧格式拒绝。
- 代际失配、重复slot、并发split及叶部分成功不可错误释放目标。
- 持久化失败后不能复用未确认失效页或覆盖尚需恢复的描述符。
- 区间部分分配/碎片化/失败回滚，以及覆盖写不新增容量预留。
- 额度释放幂等、原生EXT4/rNVPC模式隔离和真实盘完整写入。

## Task 1: 双副本描述符和分阶段批量引擎

Files: rnvpc/layout.rs, persist.rs, fault.rs, tests.rs。
Interfaces: update_descriptors(&[(usize,NvDescriptor)], fault), clear_frames(&[usize], fault)。
- [x] RED：已有描述符更新AfterContentPersist掉电仍能读旧描述符；当前单副本失败。
- [x] 新布局版本，独立缓存线选择字/副本；稳定读选择字前后复核。
- [x] 同阶段写所有副本→persist→fence，再发布各选择字→persist→fence。
- [x] 批量清除统一fence；逐页clear及generic commit保留适配。
- [x] 故障切点/部分发布/无数据重persist/两次fence/旧格式拒绝测试GREEN。

## Task 2: 区间分配与批量目标/额度提交

Files: ext4/migration.rs, rnvpc/frame.rs, write_transaction.rs。
Interfaces: assign_migration_targets(&[(PersistentObjectRef,u64)]), mark_migration_credits_released(&[PersistentObjectRef])。
- [x] RED：连续候选应以区间分配；描述符更新次数/fence应按批次收敛。
- [x] 按inode/逻辑连续性整理候选，按返回长度处理部分分配与回滚。
- [x] 保留分配持久化→目标引用发布顺序；共享目标提交及索引更新。
- [x] 叶级结果保持独立，成功集合批量额度提交；每inode账本和运行时容量汇总。
- [x] 陈旧代际/失败/重复释放回归GREEN。

## Task 3: 批量回收与故障恢复

Files: rnvpc/frame.rs, ext4/migration.rs, rnvpc tests。
Interfaces: reclaim_after_ssd_publish_batch(Vec<(EvictingDataPage,PersistentObjectRef)>, fault)。
- [x] RED：N页回收fence一次，失效前故障不归还页。
- [x] 校验lease，批量清除，fence后在有界allocation锁窗口归还，统一通知。
- [x] 保留单页适配、部分失败隔离及代际校验；全套kernel回归GREEN。

## Task 4: 性能与证据

Files: docs/experiments中的报告及脚本快照。
- [x] release构建；rustfmt/git diff --check。
- [x] 同配置2GiB、64MiB、4KiB随机/128KiB顺序、1/16线程，nojournal，前后A/B，保持128页窗口。
- [x] 独立计数/阶段计时版本报告分配、持久化调用、fence、叶发布及分阶段延迟；不以计时版带宽替代release结果。
- [x] 最终独立审查，处理阻塞项并回归，保存限制与收益。
