# Virtio 单队列 64 / 1024 深度对照（2026-10-18）

## 结论

在已启用连续 DMA segment 合并的基础上，将队列深度从 64 提高到 1024，没有观察到持续并发度或带宽提升。增加容量可以容纳更多异步在途请求，但本次负载没有持续供给足够请求；不能据此认定单个请求转发线程已饱和。

同一个带探针内核 ISO，按 64→1024→1024→64 顺序测试，每次只改变 QEMU 测试盘 `queue-size`。驱动选择 `min(1024, device_max)`，串口实测 `vdc size=64` 或 `vdc size=1024`，所有轮次 ISO SHA256 一致。

| 指标 | 64，第 1 / 2 轮 | 1024，第 1 / 2 轮 |
|---|---:|---:|
| 驱动在途请求峰值 | 8 / 9 | 10 / 8 |
| 时间加权平均在途请求 | 0.649 / 0.573 | 0.622 / 0.567 |
| 在途请求为零的时间比例 | 62.12% / 70.90% | 71.46% / 66.96% |
| descriptor 占用峰值 | 27 / 64 | 35 / 33 |
| 因 descriptor 不足阻塞的请求数 | 0 / 6 | 0 / 0 |
| descriptor 阻塞总时间 | 0 / 0.361 ms | 0 / 0 ms |
| 软件队列等待峰值，不含刚取出的请求 | 5 / 1 | 1 / 1 |
| 软件队列等待数，dequeue 事件平均 | 0.00497 / 0.01043 | 0.00798 / 0.00751 |
| 含最终 sync 的带宽，MiB/s | 1150.6 / 1018.9 | 1018.9 / 1008.9 |

64 的第二轮虽碰到 descriptor 容量限制，阻塞仅占测量窗口的 0.018%，不足以构成本轮主要瓶颈。1024 的峰值 descriptor 占用仅约 3.4%，尚未接近容量。

每个设备只有一个转发线程，但提交并不等待单个请求完成，因此单线程仍可产生多个并发请求。1024 两轮中，worker 等待空软件队列新请求的累计时间分别为 1.903 / 1.866 秒，窗口分别为 2.008 / 2.024 秒；驱动请求准备累计时间分别为 19.1 / 34.1 ms。该现象更支持上游供给存在空档，不能仅凭线程数量推断转发线程是主要瓶颈。当前统计不区分这些空档来自 writeback 扫描、snapshot/DMA 拷贝、journal、应用或调度，尚未定位具体原因。

2 GiB 短测有波动，不宣称 1024 带来稳定回退，也不宣称对其他负载无效。这里的深度是 descriptor 数量；连续数据请求至少占头、数据、尾三个 descriptor，1024 并不等于同时容纳 1024 个此类请求。

## 代码调整

- `kernel/comps/virtio/src/queue.rs`：原现代 virtio 路径限制 256，且 descriptor/avail/used 各只分配一页。现支持到 1024，按实际 descriptor 表与 ring 长度分别向上取整分配 DMA 页；包含尾部 event 字段空间。legacy 路径沿用已有布局。
- `kernel/comps/virtio/src/device/block/device.rs`：期望深度 1024，按 transport 上限回退；记录实际深度，用于准入、ID 空间和请求头/响应 DMA 区域。1024 请求头占四页，响应占一页。
- BIO 原始段数上限仍为 62（单个 BIO 拒绝 >=62 的既有行为不变），独立于队列深度；EXT4 聚合大小、DMA 合并策略、单队列和单 worker 均不改变。
- 设备不支持 1024 时自动使用其较小上限；QEMU 也必须显式提供 `queue-size=1024` 才能对测试盘启用 1024。
- 对测试盘而言，64→1024 的 DMA 区域总分配从 5 页变为 13 页，额外 32 KiB；另有 descriptor 跟踪表等内存开销。

## 配置和统计口径

- 专用分区 `/dev/nvme1n1p1`，每轮重新格式化 journal EXT4；delalloc 开启，rNVPC/rNVPCe/rTIO 关闭。
- 128 KiB sequential buffered write，16 jobs，2 GiB 总量；每 job 128 MiB，psync、iodepth=1、thread=1、direct=0、fsync=0。
- resident/dirty 上限 64 MiB、低水线 32 MiB，连续 writeback。
- QEMU KVM，16 vCPU / 8 GiB；单队列，cache=none、aio=threads、indirect_desc=off；无其他内核编译与性能窗口并行。
- CPU 隔离不变：用户 0–7、普通内核 8–11、writeback 12、completion 预留 13、virtio worker 14、journal 15。
- 在途请求指发布到 IRQ 回收之间的请求，包含已完成但未回收请求，不等于物理 NVMe 深度。
- 软件队列在 dequeue 后采样，原始 `software_pending_*` 包含刚取出的一项；报告统一减 1。平均为事件平均，不是时间平均。
- 临时占用直方图最后一桶记录 >=64；额外记录准确峰值。本次所有 descriptor 峰值 <=64、请求峰值 <=10，没有发生合桶，平均值与占用率为准确时间加权统计。1024 下 `exact_full_percent` CSV 字段仍表示 64 桶，不应解读为 1024 队列满；本次其为零。
- 各轮校验窗口初始/结束占用为零、提交数等于完成数、直方图时长总和等于窗口时长、fio 实际完整写入 2 GiB，e2fsck 均返回 0。

## 正确性验证与产物

新增 ktest 在 64、256、1024 三种规模下，验证实际 DMA 区域能容纳最后一个 descriptor/ring slot，并读写 DMA 区域尾部；旧的一页分配实现按预期失败，实现后与原 DMA 合并测试一起通过（virtio 共 2 项）。

完整内核回归 302 项通过。无临时探针的正常 release 在 1024 队列下完成 2 GiB CRC32C 写入、sync、卸载重挂、2 GiB 读回校验；两阶段 fio error=0，CRC_VERIFY_READ_PASS，QEMU exit=33，e2fsck exit=0。正式对照的四轮性能实验也全部通过 e2fsck。临时 queue profile 源码、依赖和 syscall 输出在性能测试后移除。

实验目录 `/home/zn/asterinas-build-env/ext4-qdepth-20261018` 保留脚本、完整日志、ISO、探针补丁与快照、机器可读统计及 manifest。`run-comparison.sh` 执行四轮；脚本会格式化已授权的测试分区。`pilot-depth64-*` 是旧 64 镜像预检，不计入正式对照。

[带宽 CSV](EXT4-VIRTIO-QDEPTH-2026-10-18-bandwidth.csv)；[队列 CSV](EXT4-VIRTIO-QDEPTH-2026-10-18-queues.csv)。本次未提交或推送。
