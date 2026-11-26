# NVLog 对比实验：实现约束与阶段结论

> **Archived:** 2026-11-20。本文是 NVLog 临时 smoke、崩溃复现和修复验证
> 产物的压缩归档；下文列出的正式原始数据仍单独保留。
>
> 状态：mixed-sync 和 fio bs/thread 正式矩阵均已完成。本文替代
> `nvlog-comparison-20261120` 外部工作目录中的临时 smoke、崩溃复现和修复验证数据。

## 可比性配置

- NVLog 基于仓库 `/mnt/data/zn/Rust_EXT4/NVLog` 的 Linux 5.15.125 内核。
- QEMU：64 vCPU、8 GiB DRAM，真实设备为 `/dev/nvme1n1p2` 和 `/dev/sda4`。
- NVLog 数据区固定为 128 MiB；Linux PageCache 目标为 128 MiB，总数据缓存预算
  为 256 MiB。
- PageCache 采用 cgroup v2 `memory.high=136 MiB` 触发回收和节流，
  `memory.max=512 MiB` 只作为失控保护；每点记录 `memory.stat` 和
  `memory.events`。
- mixed-sync 使用 NVMe 8 virtqueue、SATA 1 virtqueue，与对应 rNVPCe-SU
  实验一致。fio full-e2e 两种设备均使用 4 virtqueue，与历史 full-e2e
  rNVPCe-SU 矩阵一致。

## 实验中发现并修复的 NVLog 问题

真实 mixed-sync 负载最初在 `wb_log_work_fn` 稳定触发 bad-page-state 和内核
panic。根因是 `nvpc_log_page_writeback()` 把没有引用计数保护的
`struct page *` 保存到异步 work；页面可在 worker 执行前回收，worker 随后才
`get_page()`，构成 use-after-free。panic 页的 LRU 指针已为 `dead...`，调用栈位于
`wb_log_work_fn`，与该生命周期窗口一致。

修复原则：

1. 异步 work 只保留 inode 和 page index，不保留裸 page 指针；
2. worker 通过 `find_get_page(inode->i_mapping, index)` 获取带引用的当前页面；
3. work 成功入队时持有 inode 引用，结束时 `iput()`；重复入队失败时立即释放。

修复后，同一 8 线程、30:70 读写、40% `fsync` 点连续三轮通过，带宽为
`233.77/246.47/222.95 MiB/s`；三轮均正常 sync、关闭、卸载和 e2fsck，且无内核
warning/panic。

另一个环境问题是把 `memory.max` 直接设为 136 MiB。NVLog 暂时 pin 住脏页时，
硬上限会 OOM-kill 基准进程。改用 `memory.high=136 MiB` 后，同一 0% `fsync`
验证点文件缓存峰值为 `132,993,024` 字节，`oom=0`、`oom_kill=0`。因此缓存预算
必须用软节流阈值实现，不能把目标容量直接当作 memcg 硬上限。

## mixed-sync 正式结果

正式目录 `mixed-sync-v3` 共 `144/144` 个 NVLog 点有效：NVMe/SATA 各 72 点，
每个坐标三轮。所有 benchmark、sync、卸载、QEMU 和 e2fsck 状态为 0；文件缓存
峰值为 `125.43--129.23 MiB`，均位于 128 MiB 目标的 10% 以内。

下表为三轮平均带宽的代表点：

| 设备 | R/W | sync | EXT4 | rNVPCe-SU | NVLog |
| --- | ---: | ---: | ---: | ---: | ---: |
| NVMe | 0/100 | 0% | 260.9 | 558.2 | 562.8 MiB/s |
| NVMe | 0/100 | 100% | 27.6 | 550.2 | 153.9 MiB/s |
| NVMe | 30/70 | 100% | 36.5 | 412.3 | 181.5 MiB/s |
| NVMe | 50/50 | 100% | 50.5 | 330.9 | 202.7 MiB/s |
| SATA | 0/100 | 0% | 227.0 | 191.2 | 229.7 MiB/s |
| SATA | 0/100 | 100% | 20.6 | 189.8 | 56.8 MiB/s |
| SATA | 50/50 | 100% | 36.9 | 183.8 | 58.4 MiB/s |

结论：NVLog 在无同步纯写时接近或超过基线；同步比例升高后仍优于 EXT4，但明显
低于 rNVPCe-SU。NVMe 混合读写且 sync=0% 的部分点出现反常低吞吐，例如
R/W=70/30 仅 `42.7 MiB/s`，后续应单独分析 NVLog 后台回收和无 fsync 时的脏页
状态推进，不能把它解释为设备带宽上限。

## fio 阶段验证

正式 fio 矩阵包含两设备、三负载、9 个 thread 点、9 个 bs 点和三轮重复，共
324 点；每点约 2 GiB，使用 buffered `psync`、iodepth 1。

启动前的三个验证点：

| 点 | 带宽 | PageCache 峰值 |
| --- | ---: | ---: |
| NVMe 32 线程 4 KiB 随机覆盖写 | 535.28 MiB/s | 119.27 MiB |
| NVMe 单线程 128 KiB 顺序追加 | 658.31 MiB/s | 124.63 MiB |
| SATA 单线程 4 KiB 冷随机读 | 14.79 MiB/s | 125.12 MiB |

三点均无 OOM、内核异常或文件系统错误。324 点均已完成并通过正式解析器复验。
统一逐轮数据、三轮聚合和三对象图片位于
`docs/experiments/fio/formal/full-e2e-20261123/`；不得用 smoke 或单轮
结果替代该三轮聚合。

## 数据保留边界

保留：

- 正式 mixed-sync 原始证据：`mixed-sync-v3`；
- 已完成的正式 fio 精简原始数据：
  `docs/experiments/fio/formal/full-e2e-20261123/raw/nvlog/` 中每点的
  `result.json`，以及同目录 `nvlog-results.csv`；
- 当前内核、`rootfs-v5.ext4`、构建元数据和可重建来源；
- 仓库内测试脚本与本结论文档。

删除：正式 fio 每点的 QEMU、mkfs、e2fsck 和运行时 NVDIMM 日志，旧 rootfs
版本、smoke rootfs 副本、已被正式点覆盖的 smoke 结果，以及用于定位上述两个
问题的中间矩阵。正式 fio 的 324 个最终结果已由 `result.json`、统一 CSV 和聚合
CSV 保存。
