# 008 Nojournal Delalloc Metadata Writeback

## Goal

在 writeback 内合并 nojournal delalloc 元数据持久化，不吞错误，不把元数据分配移回前台。

## Current Status

**当前 rebuild 工作树已接入 chunk 级 extent-node 合并写回，尚未提交。** 与007 delalloc一起验证；未宣称覆盖历史所有性能优化。

## Implemented Result

每个 materialization chunk 最多64页。外部 extent 节点的 pending after-image 按物理块去重；节点读取优先使用待写内容，不受事实缓存淘汰影响。离开 chunk 时统一写回，失败项保留用于显式重试。bitmap/GDT/SB 延用 dirty metadata funnel；后台 writeback 与 fsync 完成必要元数据持久化，inode 元数据失败后即使数据页已 clean 也会重试。

无 journal 的掉电原子性不在保证范围；成功 fsync 前仍须完成数据、必要元数据和设备屏障。实现边界：[EXT4-DELALLOC-2026-10-17.md](/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild/docs/recovery/EXT4-DELALLOC-2026-10-17.md)。

## Verification Evidence

完整 kernel 299 passed、0 failed；两模式稀疏/碎片外部树 remount、数据 I/O 失败重试、指定 extent-node/描述符写失败重试、后台 clean-data 元数据重试均通过。独立审查后修复并记录 RED→GREEN。

证据：`/home/zn/asterinas-build-env/ext4-delalloc-20261017/`。本轮未做 fio，不沿用历史吞吐表作为新结果。

## Remaining Work

真实掉电/完整故障矩阵、SMP、性能评估；更长周期跨 chunk 合并以及 bitmap/GDT 的进一步批量化需要独立评估，不能牺牲失败重试和同步边界。
