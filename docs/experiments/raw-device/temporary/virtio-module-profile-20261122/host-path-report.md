# virtio-blk 主机完成路径延迟分解（2026-11-22）

## 结论

32 jobs 下，guest 探针测得“请求发布到完成 IRQ”约 118.98 us。单独观察
星绽时，QEMU 10.2 的请求派发、工作线程完成和协程恢复是最大的可见窗口；
但补充 Linux 同配置、同探针对照后，可以排除“星绽使 QEMU 单请求处理明显
变慢”这一解释：

- `virtio_blk_handle_request -> virtio_blk_rw_complete` 为 68.65 us；
- 其中宿主 `pwrite64` 仅 21.79 us；
- 星绽 QEMU 非 `pwrite64` 的派发/等待/恢复窗口约为 46.87 us，但 Linux
  同阶段也接近；
- `pwrite64` 内部的 NVMe 服务仅 8.27 us，并未随并发显著增长；
- IOThread wakeup 到实际运行从 4 jobs 的 0.67 us 增到 32 jobs 的
  3.33 us，是次要并发开销；
- guest vCPU wakeup 到实际运行约 0.7--1.3 us，不是主要瓶颈。

按未插桩 32 jobs 带宽 557.99 MiB/s（约 142.8 KIOPS）和 8.27 us 的设备
服务时间估算，宿主 NVMe 平均硬件在途深度约为 1.18。Linux 未插桩约
1213 MiB/s，对应深度约 2.65。两者的 QEMU 单请求阶段只差数微秒，却有约
两倍吞吐差距，因此首要问题是星绽 guest 提交、等待和完成唤醒流水不能持续
向 QEMU 供给请求，而不是 QEMU 为星绽执行了明显更慢的单请求处理路径。

## 配置

- `/dev/nvme1n1p2`，4 KiB 随机写；
- `psync`、`direct=1`、每 job `iodepth=1`；
- 4、16、32 jobs；8 virtqueue、8 QEMU IOThread；
- QEMU 10.2.1，raw backend：`cache=none,aio=threads`；
- 短探针每点 64 MiB，16,384 个目标写请求；
- IOThread 固定到宿主 CPU 56--63。

请求级关联使用 QEMU 的 `VirtIOBlockReq *`：

```text
virtio_blk_handle_vq
  -> virtio_blk_handle_request(req)
  -> pwrite64 / block_rq_issue / block_rq_complete
  -> virtio_blk_rw_complete(req)
  -> virtio_blk_req_complete(req)
  -> virtio_notify_irqfd_deferred_fn
```

## 同请求 QEMU 阶段

下表来自同一轮 BPF 请求指针关联。`handle -> rw complete` 包含 QEMU
block backend、线程池、宿主写系统调用和完成后协程恢复。

BPF probe 会改变该短实验的绝对带宽，因此本文不使用 probe 运行的 fio 带宽
替代未插桩结果；阶段间的同请求时间差用于定位窗口，未插桩实验用于报告带宽和
估算硬件在途深度。

| jobs | handle -> rw complete us | rw complete -> req complete us | req complete -> deferred IRQ us | 三段合计 us |
|---:|---:|---:|---:|---:|
| 4 | 32.37 | 2.29 | 3.32 | 37.98 |
| 16 | 43.57 | 3.17 | 3.77 | 50.51 |
| 32 | 68.65 | 2.91 | 3.52 | 75.08 |

32 jobs 相比 4 jobs，增长集中在第一段：`handle -> rw complete` 增加
36.28 us；响应发布和延迟 IRQ 阶段基本稳定。

## 宿主写入分解

`pwrite64`、block issue 和 complete 使用同一目标 fd、设备号和工作线程关联。
少量 block 请求因 sector 重复或块层合并未能一一关联，因此 block 子阶段样本数
略少于 16,384；`pwrite64` 样本完整。

| jobs | pwrite64 us | 内核提交 us | NVMe 服务 us | 内核完成返回 us | QEMU 非 pwrite64 us |
|---:|---:|---:|---:|---:|---:|
| 4 | 15.84 | 1.48 | 8.60 | 4.56 | 16.53 |
| 16 | 20.71 | 2.21 | 8.31 | 9.24 | 22.86 |
| 32 | 21.79 | 1.96 | 8.27 | 10.45 | 46.87 |

这里的“QEMU 非 pwrite64”定义为 `handle -> rw complete` 减去
`pwrite64`。32 jobs 时它是 46.87 us，占该阶段的 68.3%，是当前最大的可定位
软件窗口。NVMe 服务时间反而从 8.60 us 略降到 8.27 us，因此设备介质和硬件
队列服务不是并发延迟增长来源。

## 调度和批处理

| jobs | IOThread wakeup -> run us | vq callback -> request us（趋势值） | 每 deferred IRQ 完成请求数 |
|---:|---:|---:|---:|
| 4 | 0.67 | 3.10 | 1.01 |
| 16 | 2.10 | 4.81 | 1.43 |
| 32 | 3.33 | 15.96 | 2.54 |

32 jobs 时每个 virtqueue 平均承载 4 个同步线程。`virtio_blk_handle_vq` 一次回调
处理的请求更多，后续请求在同一 IOThread 中串行排在前面的请求之后，因此
`vq callback -> request` 窗口增长。IRQ 合并则随并发增强，每次 deferred IRQ
覆盖约 2.54 个请求；IRQ 调用次数下降是合并结果，不是完成丢失。

vq 趋势值来自单独一轮探针。加入 vCPU scheduler probe 会明显降低带宽，因此
vCPU 数据只用于排除“宿主没有及时调度 guest vCPU”这一解释，不与请求级阶段
做精确加和。

## 对 118.98 us 的解释

32 jobs 下，主机可见的路径约包含：

```text
vq callback 到 request           约 16.0 us（趋势值）
request 到 backend 完成          68.7 us
backend 回调到 request 完成       2.9 us
request 完成到 deferred IRQ       3.5 us
------------------------------------------
主机已定位部分                    约 91 us
```

剩余部分包括 guest notify 到 QEMU vq callback、irqfd/KVM 注入到 guest IRQ
入口，以及不同探针运行和探针扰动造成的差值。现有证据足以确定主要优化目标，
但不把这个差值解释成单一函数的精确延迟。

## 瓶颈排序

1. **QEMU `aio=threads` 派发与完成恢复**：32 jobs 约 46.87 us，且相对
   4 jobs 增长 30.34 us。
2. **同一 IOThread 内的 virtqueue 批处理串行等待**：32 jobs 趋势值约
   15.96 us，明显高于 4 jobs 的 3.10 us。
3. **宿主内核完成返回**：32 jobs 10.45 us，相对 4 jobs 增加 5.90 us。
4. **IOThread 调度**：32 jobs 3.33 us，是可测但次要的增长项。
5. **NVMe 服务**：约 8.3 us 且稳定，排除为主要瓶颈。

## Linux 对照

Linux 使用相同的 QEMU 10.2.1、8 virtqueue、8 IOThread、`aio=threads`、
QEMU设备参数、NVMe分区和4 KiB `psync`负载。fio线程按照Linux blk-mq的
8组CPU映射分散，探针和星绽的vq趋势复核轮次完全相同。

### 32 jobs 同路径对比

| 阶段 | 星绽 us | Linux us | Linux - 星绽 us |
|---|---:|---:|---:|
| vq callback -> request | 15.96 | 14.84 | -1.12 |
| request -> rw complete | 92.03 | 86.65 | -5.37 |
| rw complete -> req complete | 3.11 | 3.71 | +0.60 |
| req complete -> deferred IRQ | 3.49 | 4.58 | +1.09 |
| `pwrite64` | 25.83 | 23.63 | -2.20 |
| NVMe服务 | 8.92 | 8.54 | -0.38 |
| IOThread wakeup -> run | 7.82 | 7.48 | -0.34 |

QEMU单请求阶段没有数量级差异。Linux的`request -> rw complete`只比星绽短
5.37 us，不能解释两者约2倍的带宽差距；响应发布、IRQ准备、设备服务和
IOThread调度也基本相同。

### 吞吐与实际在途深度

| jobs | 星绽插桩带宽 MiB/s | Linux插桩带宽 MiB/s | 星绽估算NVMe QD | Linux估算NVMe QD |
|---:|---:|---:|---:|---:|
| 4 | 188.24 | 198.76 | 0.39 | 0.40 |
| 16 | 265.56 | 528.93 | 0.58 | 1.11 |
| 32 | 251.97 | 551.72 | 0.58 | 1.21 |

估算QD使用`IOPS * block_rq_issue到complete平均时间`。4 jobs时两者一致；
从16 jobs开始，Linux的实际硬件在途深度约为星绽两倍，而单个NVMe请求的
服务时间仍约为8--9 us。这说明Linux不是让QEMU更快地处理单个请求，而是更
持续地向QEMU和NVMe提供并发请求。

历史未插桩长测也呈现同一趋势：32 jobs下Linux约1213 MiB/s，星绽缓存容量
查询修复后约558 MiB/s。Linux对应的平均NVMe在途深度约2.65，星绽约1.18。

因此，“星绽的QEMU侧单请求处理效率低下”这一假设不成立。剩余瓶颈位于QEMU
边界之前或完成返回guest之后，重点是星绽virtio驱动的队列选择、同步等待与
唤醒流水，以及每个线程发出下一请求前的guest侧空闲时间。

## 数据位置

- 请求级有效探针：`/home/zn/asterinas-build-env/virtio-host-path-bpf6-20261122/results/`
- vq callback 趋势复核：`/home/zn/asterinas-build-env/virtio-host-path-bpf9-20261122/results/`
- vCPU 调度消融：`/home/zn/asterinas-build-env/virtio-host-path-bpf8-20261122/results/`
- Linux同探针对照：`/home/zn/asterinas-build-env/virtio-linux-host-path-bpf-20261122/results/`
- 诊断脚本快照：`scripts/host-path/`

全文本 QEMU trace 会把带宽压低到约 85 MiB/s，故只用于确认事件顺序，不用于
延迟结论。最终表格使用 BPF 聚合数据。

历史 Linux raw 对照同样关闭了 `event_idx`、indirect descriptor 和 request
merging，因此当前差距不能简单归因于这些 QEMU 开关。
