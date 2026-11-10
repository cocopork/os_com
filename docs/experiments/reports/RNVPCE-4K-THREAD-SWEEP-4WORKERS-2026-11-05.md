# rNVPCe 默认四 worker 的 4 KiB 随机覆盖写 thread sweep（2026-11-05）

## 结论

rNVPCe 默认迁移 worker 数已从 1 改为
`min(4, submission_queue_count)`。本轮镜像未显式传入 `rtio_workers`，
四个 worker 仍分别通过 virtqueue 0--3 到达宿主 NVMe hctx 0--3，说明新默认配置
实际生效，而不是依赖测试参数覆盖。

相对同日单 worker 基线，8/16-job 的 fio 带宽分别提高 57.5%/90.3%，达到
295.10/348.89 MiB/s；16-job 平均延迟从 326.43 us 降至 132.53 us，p99 从
11.99 ms 降至 618.50 us。宿主 hctx 最大占比降至 25.1%--47.8%，所有测试点
均通过多 hctx 分布门槛。此前约 96% 请求集中在 hctx 0 的原因确实是默认只启动了
worker 0，而不是四个 worker 都被驱动转发到队列 0。

该变化并非全场景提升：1/2-job 带宽分别下降 11.6%/7.7%，2-job 仍只有
99.53 MiB/s，明显低于 1-job。后续 2×2 交叉实验已经确认，该突降来自测试准备阶段
并发创建文件产生的高度碎片化 SSD baseline，而不是双线程调度或 I/O 机制切换；连续
布局的 2-job 达到 350.27 MiB/s、21.95 us。详见
[文件布局交叉实验](RNVPCE-LAYOUT-CROSS-2026-11-05.md)。

## 配置变更

- rNVPCe 默认：`min(4, submission_queue_count)`；
- 显式 `rtio_workers=N`：继续覆盖默认值，并校验 `1 <= N <= 16` 且不超过设备队列数；
- 单队列设备：默认仍为一个 worker；
- rNVPC WAL：仍固定为一个 worker；
- profiling 与 latency instrumentation：关闭。

## 测试配置

- 分支：`optimize/rnvpce-allocation-goal`，包含当前工作区未提交实现；
- release ISO SHA-256：
  `f04ff9c241fe7187d81bedd67bef4f54a120ea9bd912dcaf2c36326505c2a432`；
- 内核命令行未包含 `rtio_workers`；
- 设备：`/dev/nvme1n1p2`，每个点重新格式化为 4 KiB nojournal EXT4；
- 来宾：64 vCPU、8 GiB、4 个 virtqueue/IOThread；
- rNVPCe：128 MiB pool，64 MiB metadata reserve，64 MiB data limit，fast 模式；
- fio：4 KiB `randwrite`、buffered `psync`、iodepth 1/job、每线程独立文件；
- job 数：`1/2/4/8/16`，每点精确写入 2 GiB；
- 每点先准备文件并完成一次 warm overwrite，再测量第二次随机覆盖及 sync；
- 每点仅运行一次，属于开发阶段性能扫描，不提供统计置信区间。

## 测试结果

| job | fio MiB/s | 含 sync MiB/s | IOPS | 平均延迟 us | p99 us | 最大 hctx 占比 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 176.75 | 170.10 | 45,248 | 21.74 | 42.75 | 47.81% |
| 2 | 99.53 | 97.90 | 25,479 | 76.43 | 113.15 | 26.22% |
| 4 | 187.08 | 179.02 | 47,893 | 80.95 | 126.46 | 25.10% |
| 8 | 295.10 | 276.01 | 75,546 | 100.23 | 220.16 | 42.74% |
| 16 | 348.89 | 328.73 | 89,317 | 132.53 | 618.50 | 30.86% |

## 与单 worker 基线对比

对照数据来自同日、同设备和相同 workload 的单 worker 扫描。

| job | 单 worker MiB/s | 四 worker MiB/s | fio 变化 | 单 worker 含 sync | 四 worker 含 sync | 含 sync 变化 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 200.04 | 176.75 | -11.6% | 190.16 | 170.10 | -10.5% |
| 2 | 107.86 | 99.53 | -7.7% | 105.73 | 97.90 | -7.4% |
| 4 | 180.15 | 187.08 | +3.8% | 173.41 | 179.02 | +3.2% |
| 8 | 187.39 | 295.10 | +57.5% | 178.86 | 276.01 | +54.3% |
| 16 | 183.35 | 348.89 | +90.3% | 176.25 | 328.73 | +86.5% |

## 队列分布与正确性

| job | 活跃 hctx 数 | 最大 hctx 占比 | 分布验收 | runner | fio/sync/umount | e2fsck |
|---:|---:|---:|:---:|---:|:---:|---:|
| 1 | 6 | 47.81% | 通过 | 0 | 0 / 0 / 0 | 0 |
| 2 | 6 | 26.22% | 通过 | 0 | 0 / 0 / 0 | 0 |
| 4 | 6 | 25.10% | 通过 | 0 | 0 / 0 / 0 | 0 |
| 8 | 6 | 42.74% | 通过 | 0 | 0 / 0 / 0 | 0 |
| 16 | 6 | 30.86% | 通过 | 0 | 0 / 0 / 0 | 0 |

有效负载位于 hctx 0--3；另外两个 hctx 只有 readiness probe 的单次请求。五个点
均完成精确 2 GiB 写入，fio、sync、卸载和只读 `e2fsck -fn` 均成功。

## 证据位置

- 原始数据：
  `/home/zn/asterinas-build-env/rnvpce-thread-sweep-4workers-20261105/results`；
- 输入镜像：
  `/home/zn/asterinas-build-env/rnvpce-thread-sweep-4workers-20261105/source`；
- 单 worker 对照报告：
  [RNVPCE-4K-THREAD-SWEEP-2026-11-05.md](RNVPCE-4K-THREAD-SWEEP-2026-11-05.md)；
- 可复现实验脚本：
  `docs/experiments/scripts/rnvpce-raw-thread-sweep-20261103/run-sweep.sh`。
