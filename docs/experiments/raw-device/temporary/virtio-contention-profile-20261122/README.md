# virtio-blk 高并发竞争与延迟分解

日期：2026-11-22。

## 结论

星绽 virtio-blk 的稳定 I/O 路径中没有一把由 8 个 virtqueue 共同持有的设备级全局锁。
队列锁、完成映射、请求 ID 分配器、描述符等待队列和 staging 队列均为 per-virtqueue。
设备的 `transport` 锁只用于初始化、回调注册和配置变更，不参与每个请求的提交或完成。

32 线程性能下降也不是队列深度不足或队列分布错误：6 个测试点的描述符等待均为 0，
8 个队列的完成数几乎完全相等，正常路径的最大 staging 深度仅为 4。

确认存在的高并发增量有两部分：

1. `VirtQueue::notify()` 的 MMIO 写延迟增加。direct-submit 下从 16 线程的 2.63 us
   增至 32 线程的 6.41 us；正常 staged 路径从 2.59 us 增至 4.77 us。这不是 Rust
   全局锁，而是 guest MMIO 到 QEMU/IOThread/backend 的共享边界。
2. 请求发布到完成 IRQ 的等待增大且波动明显。direct-submit 为 72.84/146.42/115.93 us
   （16/24/32 线程），staged 为 69.69/204.18/78.92 us，说明宿主/backend/IRQ 投递路径
   在高并发下存在排队和调度波动。

反证同样明确：IRQ 内完成处理仅约 5.89--7.88 us，没有随 32 线程出现数量级增长；
完成回调到等待任务恢复为 10.04--21.53 us，虽在 32 线程约翻倍，但无法解释 fio
平均延迟升至约 576--603 us。direct-submit 32 线程中，驱动提交、发布到 IRQ、IRQ
处理和完成后恢复合计量级约 154 us，而 fio 平均延迟为 603 us；其余约 449 us 位于
探针边界之外，包含上层 block/BIO 构造、系统调用运行机会和 guest vCPU 调度。由于各
阶段是抽样累计，该差值是定位信号，不是严格可加的单请求账本。

## 代码审计

- 正常提交先按 CPU/affinity 选择队列，再进入该队列自己的 staging FIFO；每个队列有
  一个转发线程。
- `VirtioBlockQueue` 独立持有 `SpinLock<VirtQueue>`、`SyncIdAlloc`、
  `SpinLock<BTreeMap<token, request>>`、descriptor `WaitQueue` 和 staging FIFO。
- 完成中断只锁当前 virtqueue 和当前队列的 completion map；随后同步 response、释放
  当前队列的 ID，并唤醒对应 BIO。
- `should_notify()` 含 `SeqCst` fence，`notify()` 写 virtio PCI notify BAR。它是当前
  驱动内部随并发上升最明显的提交阶段。
- BIO 完成使用各 BIO 自己的 `WaitQueue`。调度器唤醒进入任务上次运行 CPU 的 per-CPU
  runqueue；调度 runqueue 也不是全局单锁。

因此，不能把问题概括为“virtio 驱动有一把全局大锁”。更准确的结论是：per-queue
临界区很短，主要可见成本在 notify 的虚拟化边界和 request publication 到 IRQ 的
宿主完成路径；32 线程另有更大的上层/guest vCPU 调度时间需要单独插桩。

## 实验

配置：真实 `/dev/nvme1n1p2`；8 virtqueue、8 个一一映射的 QEMU IOThread，固定到
宿主 CPU 56--63；64 vCPU；4 KiB randwrite；`psync`、`direct=1`、每 job
`iodepth=1`；每点约 2 GiB，重复 3 次。探针每 1024 个写请求采样一次。

| 路径 | jobs | 带宽 MiB/s | fio mean us | p99 us | driver submit us | notify us | publish->IRQ us | IRQ completion us | completion->resume us |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| direct | 16 | 408.96 | 133.41 | 267.95 | 5.23 | 2.63 | 72.84 | 7.47 | 10.19 |
| direct | 24 | 274.17 | 334.60 | 907.95 | 8.08 | 4.68 | 146.42 | 6.39 | 12.37 |
| direct | 32 | 198.53 | 602.78 | 1444.52 | 11.27 | 6.41 | 115.93 | 7.56 | 19.22 |
| staged | 16 | 301.63 | 143.22 | 544.09 | 14.90 | 2.59 | 69.69 | 7.88 | 11.37 |
| staged | 24 | 384.98 | 211.68 | 568.66 | 4.59 | 2.43 | 204.18 | 5.89 | 10.04 |
| staged | 32 | 208.67 | 575.85 | 1400.83 | 8.24 | 4.77 | 78.92 | 6.53 | 21.53 |

staged 16 线程的 DMA prepare 样本含一次明显长尾，导致该点 submit 均值和带宽偏离；
它不用于判断 DMA prepare 的趋势。direct 消融完全绕过 staging 和转发线程，但 32
线程仍从 408.96 MiB/s 降至 198.53 MiB/s，证明转发线程不是断崖下降的根因。

## 后续定位

下一轮应在 block 层入口到 `VirtioBlockQueue::write()` 之间增加时间戳，并记录 guest
vCPU runnable/运行延迟；同时在 QEMU 侧记录各 IOThread 的 CPU 利用率、poll 命中、
线程唤醒和 backend aio 完成时间。优先级高于继续拆 per-queue 锁，因为当前锁阶段均小于
1 us，且无描述符等待。

原始结果位于 `docs/experiments/raw-device/temporary/virtio-contention-profile-20261122/`，
可复现实验脚本位于 `docs/experiments/raw-device/temporary/virtio-contention-profile-20261122/scripts/`。
