# rNVPCe 并行批量 NVPage 回收实验（2026-11-16）

## 结论

本次改动消除了 20 线程后由全局 reclaim queue、逐 entry 持久化和逐 entry 容量发布造成的性能断崖。
最终关闭插桩的 24/32 线程带宽相对旧基线分别提高 98.2% 和 62.1%。不过 32 线程仍比本轮 4 线程低
24.8%，因此结论是“回收扩展性显著改善”，不是“32 线程已完全扩展”。

## 实现

1. 每个 rTIO worker 拥有独立 reclaim queue，完成项返回提交它的 worker；前台只唤醒 worker 并等待
   容量，不再从一个全局队列逐页回收。
2. native redirect entry 以最多 128 entry 为批次，共享 pending-free 发布、selector 清除和 descriptor
   清理的持久化屏障。
3. redirect index 按 inode 批量删除；`data_ranges` owner 和 slot allocator 按 allocation domain 分区并
   批量释放；容量记账和等待者唤醒每批执行一次。
4. legacy visibility-generation-zero entry 继续走原兼容协议，未改变持久化格式。

## 测试配置

- 设备：`/dev/nvme1n1p2`，QEMU 直通真实 NVMe 分区；
- 文件：2 GiB 低碎片预创建文件，4 KiB random overwrite；
- rNVPCe data limit：64 MiB；rTIO worker：4；virtqueue：4；
- fio：psync，4/16/24/32 jobs；
- release 镜像 SHA-256：`33f78d24bf29ec4614dfcb7f0bb7e005a9753336a9d020b0a3f3a5fa64bf0790`；
- 每个点均成功写满，sync、unmount 和 `e2fsck` 退出 0。

原始结果位于：
`/home/zn/asterinas-build-env/rnvpce-parallel-batched-reclaim-final-20261116/results`。

## 性能结果

| 线程 | fio 带宽 MiB/s | 含 sync MiB/s | 平均延迟 us | p99 us | 相对旧基线 |
|---:|---:|---:|---:|---:|---:|
| 4 | 577.88 | 517.17 | 26.21 | 45.31 | +7.8%（旧 535.99） |
| 16 | 537.39 | 485.31 | 110.51 | 228.35 | -15.3%（旧 634.45） |
| 24 | 513.08 | 464.69 | 181.66 | 387.07 | +98.2%（旧 258.82） |
| 32 | 434.73 | 393.09 | 265.31 | 1044.48 | +62.1%（旧 268.20） |

16 线程的旧值来自单轮历史点，且本方案第一轮曾测得 586.48 MiB/s，因此不能把 -15.3% 全部解释为
确定性回退；但它也没有达到“低线程回退不超过 5%”的验收线。24/32 线程在两轮中均显著高于旧基线，
其中 32 线程两轮为 434.27/434.73 MiB/s，改善具有重复性。

## 插桩分解

插桩镜像仅用于定位，不与关闭插桩的绝对带宽混用。4/32 线程分别测得 699.69/530.43 MiB/s。

| 指标 | 4 线程 | 32 线程 | 判断 |
|---|---:|---:|---|
| reclaim calls / pages | 4098 / 524288 | 4144 / 524288 | 平均 127.9/126.5 页，未退化为逐页回收 |
| reclaim 累计时间 | 0.845 s | 1.312 s | 有增长，但不足以解释全部前台延迟 |
| candidate rounds / pages | 526 / 524288 | 628 / 524288 | 平均约 997/835 页，候选批次仍大 |
| max in-flight pages | 8192 | 10240 | 32 线程并未因队列过浅饿死设备 |
| capacity wait calls / 累计 | 567 / 2.654 s | 59890 / 52.857 s | 剩余扩展性差距的直接表象 |
| foreground persist 累计 | 3.199 s | 9.327 s | 32 线程共享持久化路径竞争仍明显 |
| foreground post-commit 累计 | 0.266 s | 2.320 s | 通知/记账在高并发下仍放大 |

因此，worker-local 队列和批量回收解决了 reclaim-pending 页不能及时转成容量的问题，但 32 线程的
剩余差距已经转移到前台持久化、post-commit 和由此产生的容量等待。下一轮不应继续扩大 reclaim batch，
而应分别拆分 foreground persist 的共享临界区和 post-commit 的并发通知路径。

## 正确性验证

- `cargo fmt --all -- --check`：通过；
- `git diff --check`：通过；
- `cargo osdk check --ktests --package aster-kernel`：通过（既有 warning）；
- release OSDK 构建：通过；
- 四个真实盘点的写入、sync、unmount、`e2fsck`：全部通过；
- 新增 ktest 覆盖 worker 队列隔离、allocation-domain 锁分区，以及两个 native entry 共享三次回收
  屏障/一次 domain 锁/一次 inode entry-map 锁。定向 OSDK/QEMU/KVM 测试实际执行结果为
  1 passed、0 failed、459 filtered out。
