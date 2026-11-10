# rNVPCe per-worker profiling（2026-11-10）

## 目的

对比顺序覆盖写 4、16、32 jobs，确认线程数增加后 rTIO worker 的处理速度是否下降。
本实验使用带插桩的诊断内核，结果用于定位瓶颈，不作为正式性能基线。

## 环境

- 设备：`/dev/nvme1n1p2`（QEMU 中为 `vdc`）
- 文件系统：rNVPCe + rTIO，4 个 migration worker，4 个 virtqueue
- fio：4 KiB、顺序 buffered overwrite、`psync`、每 job 一个低碎片文件
- 总数据量：2 GiB
- NVM data limit：64 MiB
- 代码基线：`aee1fb38c`
- 诊断分支：`diagnostic/rtio-worker-profiling`
- 原始数据：`/home/zn/asterinas-build-env/rnvpce-worker-profiling-20261110/`

## 端到端结果

| jobs | fio 带宽 MiB/s | 含 sync MiB/s | 平均写延迟 us | p99 us |
|---:|---:|---:|---:|---:|
| 4 | 576.41 | 476.28 | 26.54 | 48.38 |
| 16 | 589.01 | 488.78 | 104.79 | 175.10 |
| 32 | 410.17 | 365.71 | 293.27 | 774.14 |

## rTIO worker 汇总

队列均值是 worker 决策循环的事件采样均值，不是固定周期的时间加权均值。

| jobs | 提交页 | BIO | completion 页 | 迁移轮次 | productive | 平均轮次 us | dirty 均值 | in-flight 均值 | completion-pending 均值 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 | 523,314 | 16,580 | 524,288 | 1,992 | 100% | 1,733.4 | 154.8 | 184.8 | 2.5 |
| 16 | 523,973 | 14,154 | 524,288 | 2,069 | 100% | 1,171.4 | 139.3 | 194.8 | 3.8 |
| 32 | 523,412 | 16,611 | 524,288 | 2,061 | 100% | 1,280.9 | 139.0 | 190.7 | 4.0 |

32 jobs 相比 16 jobs 的平均迁移轮次只增加 9.4%，dirty 和 in-flight 均值基本不变，
四个 worker 各自完成约 131,072 页。没有出现单个 worker 过载、completion 积压或 worker
吞吐随应用线程数成比例下降。

## worker 负载均衡

| jobs | worker 0 页 | worker 1 页 | worker 2 页 | worker 3 页 | 每 worker completion 页 |
|---:|---:|---:|---:|---:|---:|
| 4 | 131,072 | 130,098 | 131,072 | 131,072 | 131,072 |
| 16 | 131,072 | 131,014 | 130,815 | 131,072 | 131,072 |
| 32 | 131,058 | 131,044 | 130,370 | 130,940 | 131,072 |

提交页数的小幅差额由 quiesced/sync 上下文补齐；completion 页数严格均衡。

## virtqueue 状态

| jobs | enqueue 后 staging 均值 | staging 峰值 | in-flight 峰值 | descriptor wait |
|---:|---:|---:|---:|---:|
| 4 | 1.009 | 5 | 199 | 0 |
| 16 | 1.022 | 7 | 17 | 0 |
| 32 | 1.041 | 6 | 22 | 0 |

四个队列都没有 descriptor wait，staging 长期接近 1。32 jobs 没有出现 virtqueue 排队增长，
因此本轮数据不支持“virtio 队列拥塞导致 32 jobs 回退”。另一方面，16/32 jobs 的硬件
in-flight 峰值明显低于 4 jobs，说明软件并没有稳定地将更高应用并发转化为更深设备队列。

## 前台与后台对照

| jobs | 前台总耗时 / 页 us | capacity wait 次数 | flush 批次平均 us | candidate select 总耗时 ms | completion 批次平均 us |
|---:|---:|---:|---:|---:|---:|
| 4 | 25.29 | 0 | 1,732.3 | 12.84 | 9.05 |
| 16 | 102.27 | 0 | 1,169.1 | 17.04 | 11.88 |
| 32 | 291.04 | 0 | 1,278.2 | 40.37 | 16.02 |

## 结论

32 jobs 的主要退化发生在前台 write 路径，而不是容量等待、worker 负载不均或 completion
积压。前台每页耗时从 16 jobs 的 102.27 us 增长到 291.04 us，与 fio 平均延迟同步增长；
同期后台 flush 批次只慢 9.3%，不足以解释带宽从 589.01 MiB/s 降到 410.17 MiB/s。

下一步应在 16/32 jobs 间继续拆分 `nv_foreground_total`，重点检查 redirect mapping、
inode/extent publication guard、NVPage 分配域及持久化事务中的共享锁等待。virtio 侧应保留
in-flight 深度作为辅助指标，但不是当前首要瓶颈。
