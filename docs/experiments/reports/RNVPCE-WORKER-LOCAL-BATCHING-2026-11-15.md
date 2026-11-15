# rNVPCe worker 本地批处理与压力重平衡实验

日期：2026-11-15

## 结论

本轮实现完成且正确性验证通过，但没有改善 32 线程随机覆盖写，因此不能作为性能
优化验收通过。

普通迁移现在只看 owner worker 的本地 dirty 页数，达到 1024 页才启动；容量或显式
进度压力可以唤醒全部 worker，并允许空闲 worker 安全接管一个完整 queued inode。
durability 唤醒改为 generation 边沿触发，不再因不足水位的 ready batch 持续空转。

五轮正式结果相对修改前基线为：

| 线程 | 修改前均值 MiB/s | 本轮均值 MiB/s | 变化 | 本轮中位数 | 本轮标准差 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 588.80 | 584.67 | -0.7% | 636.02 | 129.53 |
| 16 | 382.42 | 375.47 | -1.8% | 388.25 | 38.04 |
| 32 | 222.35 | 213.97 | -3.8% | 199.53 | 25.26 |

结果说明 total dirty 的 worker 本地水位不是当前 32 线程瓶颈的正确控制量。

## 实现

### 本地激活

- 无压力时，`request_nv_migration_for_inode()` 只查询 inode owner 及该 worker 的
  `queued_dirty_pages`；不足 1024 页不唤醒。
- worker 只在自己的 dirty 页达到 1024 时进行普通迁移。
- 容量等待、显式进度等待、sync、卸载和失败继续使用全局唤醒。

### 压力重平衡

- 只在 helper 本地无 queued 页且存在容量/进度压力时检查其他 worker。
- 一次只转移一个完整 inode。
- 必须满足 owner 未变、work state 为 `QUEUED`，且 in-flight、completion-pending、
  reclaim-pending 全部为零。
- owner generation、worker dirty/owned 计数和新队列节点在 inode migration 锁保护下
  一并更新；普通迁移不扫描其他 worker 队列。

### durability 边沿

- wait predicate 不再把 `ready_batch_count != 0` 当作持续成立条件。
- BIO completion 增加 generation 并唤醒 owner；同一 generation 只消费一次。
- ready 页达到 1024 时推进普通 epoch；全局 queued dirty 和 pending data 都为零时
  推进不足水位的尾 epoch。

## 正确性验证

- `cargo fmt --all -- --check`：通过；
- `cargo osdk check --package aster-kernel`：通过；
- `cargo osdk check --ktests --package aster-kernel`：通过；
- 本地水位和 owner-only 唤醒 ktest：通过；
- queued inode 转移、RUNNING/in-flight/completion/reclaim 拒绝 ktest：通过；
- durability generation、1024 页水位和 idle-tail ktest：通过；
- 15 个正式性能点：fio 完整写入 2 GiB，sync、卸载、e2fsck 全部成功；
- 每点 hctx 检查通过，四个目标队列均有请求。

第三轮第一次启动时，宿主 trace 未捕获 readiness read，guest 尚未解除暂停即被
harness 终止。该次没有启动 fio，原始目录单独保留，不计入样本；重跑后通过。

## 实验条件

- 设备：真实 `/dev/nvme1n1p2`；
- 虚拟机：64 vCPU，4 个 rTIO worker，4 个 virtqueue/IOThread；
- 文件：2 GiB 低碎片预建文件；
- rNVPC：128 MiB 总池，64 MiB metadata reserve，64 MiB data limit，fast persist；
- 负载：4 KiB `randwrite` buffered overwrite，`psync`，4/16/32 jobs；
- 每个线程点重复五轮并轮换顺序；只比较 fio 期间带宽。

release ISO SHA256：
`9ab2f7325eb828d61ad3a7c13ce70f0630d37303d3bc18854fa1da5f9c1a07e3`。

## 五轮数据

| 轮次 | 4线程 MiB/s | 16线程 MiB/s | 32线程 MiB/s |
| --- | ---: | ---: | ---: |
| 1 | 636.02 | 404.42 | 199.53 |
| 2 | 677.02 | 388.25 | 247.67 |
| 3 | 638.01 | 373.18 | 191.80 |
| 4 | 615.94 | 311.01 | 234.30 |
| 5 | 356.36 | 400.47 | 196.56 |

## 小批次诊断

启用现有只读 pipeline profile 后，各点均迁移 524,288 页：

| 线程 | completion batches | 平均页/批 | durability epochs | ready-inode dequeues |
| ---: | ---: | ---: | ---: | ---: |
| 4 | 526 | 996.7 | 372 | 527 |
| 16 | 542 | 967.3 | 362 | 1,620 |
| 32 | 27,132 | 19.3 | 445 | 144,503 |

32 线程下每个 worker 的 completion batch 数分别为
`11,696/6,454/6,556/2,426`，提交页数也从 100,567 到 161,199 不等。设备侧四个
主要 hctx 均有流量，因此主要差异仍发生在文件系统候选变为可 claim 的软件路径。

当前 worker 水位使用 `queued_dirty_pages`，它同时包含可迁移 `Dirty` entry 和正在由
前台扩展、暂时不能执行 `Dirty -> Migrating` 的 entry。32 线程并发使后者显著增加：
total dirty 已超过 1024，但一个 claim round 实际只能取得少量 ready entry，随后
extension 完成又触发下一轮，形成大量约 19 页的小批次。

## 被否决的 stale 队列连续清理

诊断期间还测试了让 `pop_ready_inode()` 在一次调用中连续跳过 stale queue node。
该改动的 ktest 通过，但最终五轮均值降为：

| 线程 | 均值 MiB/s | 相对修改前基线 |
| ---: | ---: | ---: |
| 4 | 515.48 | -12.5% |
| 16 | 370.75 | -3.1% |
| 32 | 194.91 | -12.3% |

该实现已撤销。它集中消耗 CPU 清理 stale 节点，却没有增加当前真正可 claim 的 entry，
不是 32 线程问题的解决方案。

## 后续方向

后续应将激活控制量改为每 worker 的 `claimable_ready_pages`，而非继续调整
`queued_dirty_pages` 阈值。该计数必须在以下状态边精确更新：entry 首次发布、extension
commit/abort、候选弹出、claim 失败回滚、lease drop、owner 整体转移和最终回收。
在完成这些状态机测试前，不应直接实现，以免造成漏迁移、重复迁移或 worker 计数漂移。

## 原始证据

- 修改前五轮：`/home/zn/asterinas-build-env/rnvpce-dirty-entry-repeat-20261115/`；
- 本轮五轮：`/home/zn/asterinas-build-env/rnvpce-worker-local-batching-20261115/round-1..5/`；
- profiling：`/home/zn/asterinas-build-env/rnvpce-worker-local-batching-20261115/profile/`；
- 被否决实验：`/home/zn/asterinas-build-env/rnvpce-worker-local-batching-20261115/final-round-1..5/`；
- 启动前 trace 失败：`round-3/failed-readiness-attempt/`。
