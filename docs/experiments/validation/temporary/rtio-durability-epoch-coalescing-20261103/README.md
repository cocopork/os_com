# rTIO durability epoch 自适应聚合实现与验证

日期：2026-11-03

## 结论

023 已实现无固定定时器的 durability epoch 聚合。机制目标达成，但性能验收仅部分通过：

- 四 worker、4 KiB 随机写、16 job 的 epoch/FLUSH 从 70,790 降至 1,791，
  含 sync 带宽从 309.37 提升至 320.50 MiB/s；
- 四 worker、128 KiB 顺序写、16 job 的 epoch/FLUSH 从 14,307 降至 2,549，
  fio 阶段略升 0.9%，但含 sync 带宽下降 11.5%；
- 所有八个测试点均通过 fio、sync、umount、只读 e2fsck 和 owner 队列排空检查。

因此，逐 batch FLUSH 放大已消除；新的限制是 1024 页目标造成 completion 等待和 sync
尾部延迟。代码保留该机制，后续应把聚合目标缩小或改为基于当前 batch/页规模的自适应
阈值，不应退回每个完成 batch 一次 FLUSH。

## 实现

`DurabilityArbiter` 维护：

- `pending_data_pages`：已提交但所属 batch 尚未完成全部数据 BIO 的页；
- `ready_pages`：已进入 owner data-complete 队列、尚未被 leader 快照的页。

后台 worker 在繁忙阶段仅在 ready 达到 1024 页时推进 epoch。小尾部只能在该 worker
本轮候选选择未提交任何页后推进；sync/unmount 仍使用强制入口。提交端先登记 pending，
callback 将页从 pending 转入 ready；转移顺序先发布 ready、再移除 pending，避免并发
worker 把中间状态误判为空尾部。

第一次真实设备矩阵发现，若在候选选择前使用 `pending == 0` 判断尾部，暂时无 BIO 在途
会被误判为 dirty 工作耗尽。该版本证据保留在外部 `matrix/`；修复后最终结果保留在
`matrix-final/`。

## 环境

- 分支：`recovery/rnvpce-range-write-transactions`；
- 设备：已授权并逐点重新格式化的 `/dev/nvme1n1p1`，256 GiB；
- 来宾：64 vCPU、8 GiB，Asterinas release，QEMU 10.2.1；
- 文件系统：EXT4 nojournal，rNVPCe + rTIO；
- NVPool：128 MiB，data limit 64 MiB；
- 每点：2 GiB buffered psync、iodepth 1、首次写；
- 矩阵：1/4 worker，4 KiB randwrite 和 128 KiB write，各 1/16 job；
- 最终原始证据：`/home/zn/asterinas-build-env/rtio-durability-coalescing-20261103/matrix-final`。

| 镜像 | SHA-256 |
| --- | --- |
| 1 worker | `459e0c8e30097a5fdd9e7605c4841f55acf999919b1e52f2247cb24f665cbcfb` |
| 4 worker | `40886cab091e385f6a298e4224d9f7e5882bd7516ff961e8446ff9c5e54701de` |

profiling 开关仅在构建镜像时临时启用，复制和哈希后已恢复为 `false`。

## 最终结果

| 工作负载 | worker | fio MiB/s | 含 sync MiB/s | 平均延迟 us | P99 us | epoch/FLUSH |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | 1 | 186.49 | 174.00 | 20.6 | 27.8 | 524 |
| 4 KiB randwrite, 16 job | 1 | 179.01 | 168.14 | 345.6 | 10027.0 | 512 |
| 128 KiB write, 1 job | 1 | 1680.07 | 1055.67 | 74.1 | 113.2 | 541 |
| 128 KiB write, 16 job | 1 | 1196.26 | 882.76 | 1617.0 | 5275.6 | 605 |
| 4 KiB randwrite, 1 job | 4 | 200.78 | 187.72 | 19.1 | 27.8 | 568 |
| 4 KiB randwrite, 16 job | 4 | 353.41 | 320.50 | 168.8 | 268.3 | 1791 |
| 128 KiB write, 1 job | 4 | 1773.16 | 1170.29 | 70.2 | 148.5 | 672 |
| 128 KiB write, 16 job | 4 | 1272.05 | 842.80 | 1510.7 | 5210.1 | 2549 |

## 与 022 对比

| 四 worker 工作负载 | fio 变化 | 含 sync 变化 | epoch 变化 |
| --- | ---: | ---: | ---: |
| 4 KiB randwrite, 1 job | -0.2% | +1.4% | -26.4% |
| 4 KiB randwrite, 16 job | +4.2% | +3.6% | -97.5% |
| 128 KiB write, 1 job | -3.6% | -7.4% | +5.8% |
| 128 KiB write, 16 job | +0.9% | -11.5% | -82.2% |

4 KiB/16 job 的设备 FLUSH 累计时间从 3.489 s 降至 0.107 s，但 completion queue
平均等待从约 0.14 ms/batch 增至 8.47 ms/batch，最大值从 8.18 ms 增至 99.04 ms。
128 KiB/16 job 的 FLUSH 累计时间从 0.659 s 降至 0.179 s，平均 completion 等待从
约 0.08 ms/batch 增至 0.82 ms/batch。测试中 `nv_capacity_wait` 均为零，因此当前回退
主要体现在同步尾部和已完成 batch 的驻留时间，而不是前台因容量耗尽直接休眠。

## 正确性门禁

- 八点 fio 均完整写入 2 GiB，sync 和 umount 成功；
- 八点只读 `e2fsck -fn` 状态均为 0；
- 所有点 `epochs == device_flushes`；
- 所有 worker 的 data-complete、durable-result、owned-inode、queued、in-flight 和
  completion-pending 计数最终均为零；
- 16-job 四 worker 点通过四队列/hctx 门禁。4 KiB 单文件点的单 owner/hctx 状态不作为
  正确性失败。

## 后续方向

优先验证较小目标（例如 256 页）或按 ready batch 平均页数调整目标，使 4 KiB 多 inode
仍能共享 FLUSH，同时限制 128 KiB completion 驻留时间。新策略仍应使用候选选择结果
判断尾部，不引入固定毫秒定时器。
