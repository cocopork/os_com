# rNVPCe 4 KiB 随机覆盖写 thread sweep（2026-11-05）

## 结论

当前最终源码在 `1/2/4/8/16` 个 fio job 下分别达到
`200.04/107.86/180.15/187.39/183.35 MiB/s`。此前 allocation-goal 优化使
2-job 相比 2026-11-03 的 warm-overwrite 基线提高 87.6%，但扩展曲线仍不单调：
2-job 低于 1-job，4-job 后吞吐进入约 180--190 MiB/s 平台。

8 到 16 job 没有带宽收益，平均延迟从 163.31 us 增至 326.43 us，16-job p99
达到 11.99 ms。增加并发主要转化为排队，而不是更高吞吐。

本轮所有点的宿主请求均约有 96% 落在 NVMe hctx 0，未通过多 hctx 分布门槛。
因此本轮可以证明当前端到端路径没有随线程数扩展，但不能把高线程平台全部归因于
rNVPCe：当前 virtqueue 到宿主硬件队列的实际分布同样需要排查。

## 测试配置

- 分支：`optimize/rnvpce-allocation-goal`，包含当前工作区未提交实现；
- release ISO SHA-256：
  `ae655b67c47978b8d78fc8311feacfc0d80806294994a15f613fe97203e9aaaa`；
- profiling 和 latency instrumentation 均关闭；
- 设备：`/dev/nvme1n1p2`，每个点重新格式化为 4 KiB nojournal EXT4；
- 来宾：64 vCPU、8 GiB、4 个 virtqueue/IOThread；
- rNVPCe：128 MiB pool，64 MiB metadata reserve，64 MiB data limit，fast 模式；
- fio：4 KiB `randwrite`、buffered `psync`、iodepth 1/job、每线程独立文件；
- job 数：`1/2/4/8/16`，每点总计精确写入 2 GiB；
- 每点先用 native EXT4 direct 128 KiB 顺序写准备文件，再在同一次 rNVPCe 挂载中
  完整随机覆盖并 sync，最后测量第二次随机覆盖；
- 每点仅运行一次，属于开发阶段性能扫描，不提供统计置信区间。

## 测试结果

| job | fio MiB/s | 含 sync MiB/s | IOPS | 平均延迟 us | p99 us | 相对 1-job 吞吐 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 200.04 | 190.16 | 51,210 | 19.18 | 26.50 | 1.000 |
| 2 | 107.86 | 105.73 | 27,612 | 71.59 | 87.55 | 0.539 |
| 4 | 180.15 | 173.41 | 46,120 | 85.23 | 160.77 | 0.901 |
| 8 | 187.39 | 178.86 | 47,972 | 163.31 | 536.58 | 0.937 |
| 16 | 183.35 | 176.25 | 46,937 | 326.43 | 11,993.09 | 0.917 |

sync 增量很小：fio 与含 sync 带宽在 2--16 job 下只相差 0.1%--4.1%。这说明
被测阶段结束时没有积压大量尚未迁移的数据；本轮高线程性能平台发生在 fio 运行期间，
不是由测试结束后的集中同步造成。

## 与旧结果对比

对照为 2026-11-03 相同配置的 rNVPCe warm-overwrite 单次扫描。

| job | 旧 MiB/s | 当前 MiB/s | 带宽变化 | 旧平均延迟 us | 当前平均延迟 us |
|---:|---:|---:|---:|---:|---:|
| 1 | 205.95 | 200.04 | -2.9% | 18.63 | 19.18 |
| 2 | 57.49 | 107.86 | +87.6% | 133.34 | 71.59 |
| 4 | 116.05 | 180.15 | +55.2% | 132.72 | 85.23 |
| 8 | 202.89 | 187.39 | -7.6% | 147.17 | 163.31 |
| 16 | 260.83 | 183.35 | -29.7% | 176.90 | 326.43 |

allocation-goal 优化明显修复了 2/4-job 的重复 extent-tree 查询开销，但当前高线程
结果反而下降。这不能仅用该优化解释，因为覆盖写已经直接跳过 allocation-goal；同时，
旧实验的多线程宿主 hctx 分布较均衡，本轮则高度集中在 hctx 0。

## 队列分布与正确性

| job | hctx 0 占比 | 活跃 hctx 数 | runner 状态 | fio/sync/umount | e2fsck |
|---:|---:|---:|---:|---:|---:|
| 1 | 95.61% | 6 | 3 | 0 / 0 / 0 | 0 |
| 2 | 97.34% | 6 | 3 | 0 / 0 / 0 | 0 |
| 4 | 96.21% | 6 | 3 | 0 / 0 / 0 | 0 |
| 8 | 96.30% | 6 | 3 | 0 / 0 / 0 | 0 |
| 16 | 96.29% | 6 | 3 | 0 / 0 / 0 | 0 |

额外的 hctx 编号只有 readiness probe 的单次请求；有效负载主要位于 hctx 0--3。
runner 状态 3 仅表示没有满足四 hctx 分布门槛，不是 workload 失败。五个点均完成
精确 2 GiB 写入，fio、sync、卸载和只读 `e2fsck -fn` 均成功。

## 证据位置

- 原始数据：`/home/zn/asterinas-build-env/rnvpce-thread-sweep-20261105/results`；
- 输入镜像：`/home/zn/asterinas-build-env/rnvpce-thread-sweep-20261105/source`；
- 可复现实验脚本：
  `docs/experiments/scripts/rnvpce-raw-thread-sweep-20261103/run-sweep.sh`。
