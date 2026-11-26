# inode 归属 rTIO flusher 实现与性能验证

日期：2026-11-03

## 结论

022 方案已完成实现。rNVPCe 的候选选择、数据提交、完成后处理和回收按 inode owner
分配给 flusher；BIO callback 只投递 owner 本地队列；专用 rNVPCe completion 线程已
删除。真实 NVMe 的 8 个 2 GiB 点全部通过 fio、sync、umount 和只读 e2fsck。

四 worker 对多文件工作负载有效：4 KiB 随机写 16 job 的含 sync 带宽提高 77.9%，
128 KiB 顺序写 16 job 提高 14.4%。单文件仍主要由一个 owner 处理，符合设计约束。

当前主要遗留瓶颈已从集中 completion 转为 durability epoch 过碎：4 KiB/16 job 的
四 worker 路径产生 70,790 次 epoch/FLUSH，而单 worker 只有 520 次。下一步应合并
短时间内到达的 ready batch，而不是拆分单 inode owner。

## 环境与方法

- 分支：`recovery/rnvpce-range-write-transactions`；实现基线为 `2279ef8f5` 加本报告同批改动；
- 宿主设备：已授权并逐点重新格式化的 `/dev/nvme1n1p1`，256 GiB；
- 来宾：64 vCPU、8 GiB，Asterinas release，QEMU 10.2.1；
- 文件系统：EXT4 nojournal，rNVPCe + rTIO；
- NVPool：128 MiB，data limit 64 MiB；
- 每点：2 GiB buffered psync，iodepth 1，首次写；
- 对照：`rtio_workers=1`、1 virtqueue；实验：`rtio_workers=4`、4 virtqueue；
- 工作负载：4 KiB randwrite 和 128 KiB write，各 1/16 job；
- 原始证据：`/home/zn/asterinas-build-env/rtio-inode-owned-flushers-20261103/matrix-final`，不纳入 Git。

profiling 源码开关只在构建镜像时临时设为 `true`，复制和哈希后恢复为 `false`。

| 镜像 | SHA-256 |
| --- | --- |
| 1 worker | `7a2a4261ff4eb504df7829c27ab140d466377934f3c0eb85aca402302a90b92c` |
| 4 worker | `3e00216b1dee7cb98d895aaf7f33418cf2dafde1ef6b8935eea1fe7d42ff8d1c` |

## 性能结果

| 工作负载 | worker | fio MiB/s | 含 sync MiB/s | 平均延迟 us | P99 us |
| --- | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | 1 | 184.50 | 173.12 | 20.8 | 27.8 |
| 4 KiB randwrite, 1 job | 4 | 201.26 | 185.17 | 19.1 | 29.1 |
| 4 KiB randwrite, 16 job | 1 | 184.75 | 173.85 | 335.8 | 9240.6 |
| 4 KiB randwrite, 16 job | 4 | 339.30 | 309.37 | 180.3 | 252.9 |
| 128 KiB write, 1 job | 1 | 1986.42 | 1170.29 | 62.6 | 129.5 |
| 128 KiB write, 1 job | 4 | 1840.07 | 1264.20 | 67.6 | 166.9 |
| 128 KiB write, 16 job | 1 | 1186.56 | 832.52 | 1644.3 | 5472.3 |
| 128 KiB write, 16 job | 4 | 1261.08 | 952.56 | 1501.5 | 5144.6 |

四 worker 相对单 worker：

| 工作负载 | fio 变化 | 含 sync 变化 | 平均延迟变化 | P99 变化 |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | +9.1% | +7.0% | -8.5% | +4.6% |
| 4 KiB randwrite, 16 job | +83.6% | +77.9% | -46.3% | -97.3% |
| 128 KiB write, 1 job | -7.4% | +8.0% | +8.0% | +28.9% |
| 128 KiB write, 16 job | +6.3% | +14.4% | -8.7% | -6.0% |

单文件 128 KiB 的 fio 阶段仍有 7.4% 回退，但最终 sync 更快；该点表明多 worker 的
durability/reclaim 并行有收益，同时仍存在 leader 竞争和 epoch 管理开销。

## owner 与完成队列

16-job 点的完成页分布如下：

| 工作负载 | worker 0 | worker 1 | worker 2 | worker 3 | 最大/最小 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite | 127,991 | 129,569 | 142,586 | 124,142 | 1.15 |
| 128 KiB write | 136,544 | 131,968 | 129,568 | 126,208 | 1.08 |

所有点最终都满足：

- `owned_inodes=0`；
- `queued_dirty_pages=0`、`in_flight_pages=0`、`completion_pending_pages=0`；
- `data_complete_depth=0`、`durable_result_depth=0`；
- 四 worker 的本地队列峰值不超过 14 个 batch；
- 16-job 点四个 worker 都提交并完成数据。

这证明 assignment release 和 sync/unmount drain 正常。per-worker `submitted_pages` 只统计
正常 worker 上下文提交，不包含 sync exclusive 上下文代行的尾部提交；因此它可能略小于
`completion_pages`，不能据此判断丢页。

## durability 与容量等待

| 工作负载 | worker | epoch | epoch batch | device FLUSH | 最大 in-flight 页 | capacity wait 总计 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | 1 | 520 | 524 | 520 | 2048 | 4.09 s |
| 4 KiB randwrite, 1 job | 4 | 772 | 776 | 772 | 2048 | 2.52 s |
| 4 KiB randwrite, 16 job | 1 | 520 | 520 | 520 | 2048 | 71.83 s |
| 4 KiB randwrite, 16 job | 4 | 70,790 | 171,492 | 70,790 | 612 | 0 s |
| 128 KiB write, 1 job | 1 | 1735 | 1742 | 1735 | 2048 | 0 s |
| 128 KiB write, 1 job | 4 | 635 | 662 | 635 | 8864 | 0 s |
| 128 KiB write, 16 job | 1 | 10,479 | 11,039 | 10,479 | 1600 | 0 s |
| 128 KiB write, 16 job | 4 | 14,307 | 15,779 | 14,307 | 2432 | 0 s |

每个点均满足 `epochs == device_flushes`，即一个封闭 epoch 恰好一次 FLUSH。协议正确，
但 4 KiB/16 job 的 epoch 数过高，说明各 worker 频繁抢到 leader 后立即封闭很小的 epoch。
这是后续最值得优化的阶段。

## virtqueue 与宿主 hctx

两个 16-job 点都通过四队列和宿主 hctx 门槛：4 KiB 随机写最大 hctx 占比 27.15%，
128 KiB 顺序写为 29.82%。4 KiB/1 job 只有 worker 0 提交数据，hctx 0 占 91.59%，
因此旧的“四 hctx 且最大占比不超过 70%”门槛返回状态 3；这符合单 inode 只使用一个
owner 的设计，不是正确性失败。该点的 owner 队列和块队列均已排空。

## 实验中发现并修复的问题

1. 多 entry 批量激活原先先把 entry 设为 Dirty，再登记 owner 页数。worker 可看到
   半登记 entry 并报 `ESTALE`。现在在同一个 inode entry-map 临界区内完成状态切换和
   调度登记；新增 ktest 覆盖该顺序。
2. fio JSON 与内核 profiling 共用串口会字符级交错。guest 现在先把 JSON 写入临时文件，
   sync 和 umount 完成后才输出标记区间；解析器仍严格校验 2 GiB、fio/sync/umount 和
   e2fsck，不通过清洗掩盖业务错误。

修复前的失败证据保留在同一外部根目录的 `matrix/` 和 `matrix-fixed/`，最终结论只使用
`matrix-final/`。

## 验证清单

- focused diagnostics ktest：1/1；
- batch activation ordering ktest：1/1；
- `ext4_research_routes`：1/1；
- `cargo osdk check --package aster-kernel`：通过；
- `cargo osdk check --ktests --package aster-kernel`：通过；
- harness self-test：32/32；
- 真实设备：8/8 fio、sync、umount、`e2fsck -fn` 通过；
- profiling 源码常量已恢复为 `false`。
