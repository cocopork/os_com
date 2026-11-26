# rNVPCe inode worker 引用缓存实验（2026-11-16）

## 结论

redirect entry 发布和扩展原来会按 entry 获取全局 `migration_workers` 表锁。当前并行
批量回收版本中，将 `MigrationWorkerSet` 引用缓存到 per-inode redirect map 后，4 KiB
随机覆盖写的 32 线程两轮带宽为 524.32 和 657.89 MiB/s，均值 591.10 MiB/s。相对
最近并行回收基线的 434.73 MiB/s 提高 36.0%，平均延迟由 265.31 us 降至两轮均值
192.91 us。

该结果说明全局 worker 表锁在当前架构中已经成为前台 `persist` 的实际共享热点。它与
2026-11-12 旧架构下 per-inode 缓存的 32 线程负结果不同；期间 worker-owned migration、
dirty-entry 调度和并行批量回收均已重构，不能用旧实验否定当前结果。

## 实现

- `InodeRedirectMap` 缓存其文件系统对应的 `Arc<MigrationWorkerSet>`；
- redirect 发布、扩展、abort requeue 和回收移除使用 inode 本地引用，不再按 entry 查询
  全局 worker 表；
- worker 集合重新配置时刷新已有 inode map 的缓存；新 inode 首次访问全局表后再缓存；
- ktest-only 查表计数验证同一 inode 的第二个 entry 不再访问全局表。

Fast 模拟器的全局 persist/fence 计数未在本轮修改。既有单变量实验中关闭它们只让32线程
带宽提高1.6%，且会改变统计接口语义，不应与本次有效改动混合。

## 性能配置

- 真实设备 `/dev/nvme1n1p2`，QEMU 直通；
- 4 KiB随机覆盖写、psync、buffered I/O、每线程独立低碎片文件；
- 每点总量2 GiB，线程数4、16、32；
- 128 MiB rNVPC池、64 MiB metadata reserve、64 MiB data limit；
- fast NVDIMM模拟、4个rTIO worker、4个virtqueue；
- 每轮每点重新格式化，完成sync、卸载和e2fsck。

## 结果

| 线程 | 最近基线 MiB/s | 候选第1轮 | 候选第2轮 | 候选均值 | 均值变化 | 基线平均延迟 us | 候选平均延迟 us |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 | 577.88 | 621.55 | 586.99 | 604.27 | +4.6% | 26.21 | 24.55 |
| 16 | 537.39 | 612.26 | 647.69 | 629.97 | +17.2% | 110.51 | 93.06 |
| 32 | 434.73 | 524.32 | 657.89 | 591.10 | +36.0% | 265.31 | 192.91 |

两轮候选的fio、sync、卸载和e2fsck状态均为0。原始数据位于：

- `/home/zn/asterinas-build-env/rnvpce-worker-cache-20261116/results-valid/`；
- `/home/zn/asterinas-build-env/rnvpce-worker-cache-20261116/results-valid-r2/`。

## 验证

- 新增测试先在旧实现上失败：第二个同inode entry使全局查表次数从1增加到2；实现缓存后
  通过；
- `cargo fmt --all -- --check`：通过；
- `git diff --check`：通过；
- `cargo osdk check --ktests --package aster-kernel`：通过，只有既有warning；
- release构建和六个真实盘性能点：通过。

完整kernel ktest同时发现当前未提交并行回收工作树中的既有不一致，包括allocation-domain
数量/槽位顺序、persist调用次数、generation测试，以及
`rnvpce_extension_accounting_stays_on_atomic_fast_path`。最后一项要求扩展不获取inode migration
锁，但当前 `account_dirty_extension()` 本身显式获取该锁；这些失败并非新增worker缓存测试的
失败，本轮未扩大范围修改。
