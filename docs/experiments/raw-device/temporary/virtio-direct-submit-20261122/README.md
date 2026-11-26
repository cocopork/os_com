# 星绽 virtio-blk I/O 上下文直接提交实验

## 目的

验证星绽是否可以像 Linux 块层一样，由发起 I/O 的线程直接把 bio 提交到目标 virtqueue，并判断该路径能否改善裸设备 4 KiB 随机写的多线程扩展性。

## 原型

实验分支 `diag/virtio-direct-submit` 在 virtio-blk 设备中加入启动开关 `virtio_blk_direct_submit=on`：

1. 沿用现有 CPU/affinity 到 virtqueue 的选择规则；
2. 绕过 per-queue staging queue 和转发线程；
3. 在调用线程上下文中构造 virtio 请求并提交到对应 virtqueue；
4. 沿用每队列独立的 virtqueue 锁、request ID、completion map 和 IRQ completion；
5. virtqueue 描述符不足时，由当前 I/O 调用线程在该队列的等待队列上等待。

原有 staging 路径保持不变，便于使用相同设备、队列和负载做启动参数消融。

## 配置

- 真实设备：`/dev/nvme1n1p2`，QEMU raw 直通；
- 8 virtqueue、8 QEMU IOThread，IOThread 固定到宿主 CPU 56--63；
- 4 KiB `randwrite`、`psync`、`direct=1`、每 job `iodepth=1`；
- 1/2/4/8/16/24/32 jobs；
- 每点约 2 GiB、3 轮；
- 禁用 QEMU request merging、event index 和 indirect descriptors，隔离提交路径差异。

## 结果

| jobs | staging MiB/s | direct MiB/s | 吞吐变化 | staging 平均延迟 us | direct 平均延迟 us | direct CV |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 49.35 | 52.78 | +6.96% | 78.75 | 73.63 | 4.74% |
| 2 | 87.52 | 94.12 | +7.53% | 87.48 | 81.70 | 1.54% |
| 4 | 158.94 | 175.35 | +10.33% | 95.07 | 85.85 | 2.37% |
| 8 | 294.34 | 318.45 | +8.19% | 103.44 | 96.13 | 2.07% |
| 16 | 415.70 | 413.74 | -0.47% | 133.62 | 134.02 | 13.04% |
| 24 | 394.94 | 319.77 | -19.03% | 216.88 | 290.01 | 26.67% |
| 32 | 227.89 | 267.94 | +17.57% | 531.59 | 495.04 | 47.75% |

32 jobs 的 direct 三轮吞吐为 415.67、192.17、195.98 MiB/s，均值不能视为稳定提升。24 jobs 也出现明显回退和波动。

## 结论

星绽可以在 I/O 线程上下文中直接提交 bio 到 virtqueue。1--8 jobs 下，该原型稳定消除了 staging 入队、唤醒和转发上下文切换的固定开销，吞吐提高 6.96%--10.33%，平均延迟降低 6.50%--9.70%。

直接提交不能单独解决高并发扩展性问题。16 jobs 后，提交路径不再是主要瓶颈；直接路径还会让前台 I/O 线程直接承担 virtqueue 锁竞争和描述符不足等待。24/32 jobs 的高变异系数说明后续应测量各队列 descriptor wait、IRQ completion 和队列分布，而不是直接用该原型替换当前路径。

因此推荐把 direct-submit 保留为诊断开关。若继续优化正式路径，重点应是高并发下的队列选择与背压，而不是继续增加或轮询转发线程。

## 验证

- 14 个测试点、42 轮 fio 均完成预期 I/O 字节数；
- 14 次 QEMU 退出状态均为 isa-debug-exit 成功值 33；
- 14 个测试点均查询到 8 个 QEMU IOThread，并记录 CPU 56--63 绑核结果；
- 内核 ktest 验证直接路径不进入 staging queue，且 IRQ completion 能完成原 bio；
- `cargo osdk check --ktests --package aster-kernel` 通过。

精简原始数据见 `docs/experiments/raw-device/temporary/virtio-direct-submit-20261122/`。
