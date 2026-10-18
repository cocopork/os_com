# 007 Delalloc Completion

## Goal

原生 EXT4 journal / nojournal 的 buffered write 延迟物理分配，完成容量预留、ordered data、生命周期及同步边界。

## Current Status

**当前 rebuild 工作树已实现，尚未提交。** 2026-10-17，默认 delalloc；nodelalloc 保留旧路径。该结论只针对当前工作树和下列新证据，不追溯宣称历史代码已完整恢复。

## Implemented Result

ES 不可淘汰 delayed 层、持有者容量预留与原子结算；前台不启动事务/不分配数据或 extent 元数据；writeback 有界分配/转换与事务 restart；属性捕获安全大小；mmap 脏洞、truncate/punch/direct/fallocate/删除及 fsync/sync 接入。

完整实现说明：[EXT4-DELALLOC-2026-10-17.md](/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild/docs/recovery/EXT4-DELALLOC-2026-10-17.md)。实施计划见 [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)。nojournal 元数据合并写回见008。

## Verification Evidence

Docker/OSDK/QEMU/KVM，SMP=1，4GiB。完整 kernel：299 passed，0 failed；保留的570个原 EXT4 场景全部恰好通过一次。补充已提交未 checkpoint 的单页前缀模拟崩溃/replay 测试通过（1 passed）。独立只读审查的 ordered EOF、事务 restart 及元数据重试问题均有 RED→GREEN。

证据：`/home/zn/asterinas-build-env/ext4-delalloc-20261017/`，完整日志 `final-full.log`、补充恢复 `replay-final.log`、审计 `audit.py`、台账 `progress.md`。未做本轮真实设备 fio。

## Remaining Work

缩小保守的27块/未知页预留预算；SMP、多用户态负载、真实掉电与完整故障矩阵验证；性能测量。非零 truncate 等先 drain 的实现正确但可减少无用刷写；既有 inode 锁、同步 BIO 和后台 error 状态处理仍是独立优化项。
