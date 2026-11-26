# rNVPCe inode ready queue 去重实验

## 结论

最终保留 `IDLE/QUEUED/RUNNING/RERUN` inode 工作状态机。前台在 rTIO worker
处理同一 inode 时新增 dirty entry，只把状态从 `RUNNING` 改为 `RERUN`，不再把
同一 inode 重复插入 ready queue；worker 收尾时根据状态最多补入一次。

32 线程 4 KiB 随机覆盖写三轮为 `236.16 / 216.67 / 237.50 MiB/s`，均值
`230.11 MiB/s`，中位数 `236.16 MiB/s`。旧基线三轮为
`181.79 / 282.44 / 212.38 MiB/s`，均值 `225.54 MiB/s`，中位数
`212.38 MiB/s`。最终候选均值提高 `2.0%`，中位数提高 `11.2%`；平均延迟均值
从 `534.72 us` 降至 `512.25 us`，p99 均值从 `3173.03 us` 降至
`2779.82 us`。

该修改消除了一个确定的重复入队竞争，但没有突破 32 线程共享管线平台。三轮波动
仍大，不能把中位数提升解释为稳定的吞吐增益。

## 实现

原实现用一个 `ready_queued` 布尔值表示 inode 是否在 ready queue。worker 出队后
立即清除此位；若前台在 worker 仍处理该 inode 时扩展 entry，前台会再次入队。
持续并发写因此可能让一个正在运行的 inode 同时出现在 ready queue 中，增加队列锁、
dequeue 和空转开销。

新状态机具有以下转换：

| 事件 | 转换 | 是否入队 |
| --- | --- | --- |
| idle inode 产生 dirty work | `IDLE -> QUEUED` | 是 |
| worker 出队 | `QUEUED -> RUNNING` | 否 |
| 运行期间新增 dirty work | `RUNNING -> RERUN` | 否 |
| worker 收尾且需再运行 | `RUNNING/RERUN -> QUEUED` | 一次 |
| worker 收尾且无后续工作 | `RUNNING -> IDLE` | 否 |

转换使用 CAS，覆盖前台发布与 worker 收尾的交错；claim 回滚也通过同一调度入口，
不再单独操作 ready 标志。

## 被否决的 owner 定向唤醒

同时测试了普通 post-commit 只唤醒 inode owner，并只读取 owner 的 dirty-page
计数。4/16/32 线程三轮均值分别为 `590.83 / 373.08 / 206.15 MiB/s`；相对旧
基线分别为 `+14.5% / -6.7% / -8.6%`。该方案让稳定 owner 的负载不均更明显，
降低其他 worker 协同推进迁移的机会，因此已经从最终代码撤回。容量、sync、失败
等路径本来仍是全 worker 唤醒；最终普通路径也继续使用已有的逐 worker pending
边沿合并与广播语义。

## 测试条件与验证

- 真实设备：`/dev/nvme1n1p2`；每个点均重新格式化并在结束后执行 e2fsck。
- 负载：2 GiB、4 KiB `randwrite` overwrite、4/16/32 fio jobs。
- rNVPCe：128 MiB 池、64 MiB data limit、4 个 rTIO worker。
- 针对性 ktest：`rnvpce_running_inode_records_rerun_without_duplicate_queue_entry` 通过，
  验证运行期间扩展不产生第二个队列项，收尾后恰好重跑一次。
- `cargo osdk check --package aster-kernel` 和
  `cargo osdk check --ktests --package aster-kernel` 通过，仅有既存警告。
- release 构建、所有 fio 点的 sync、卸载和 e2fsck 均通过。

原始数据：

- 旧基线：`/home/zn/asterinas-build-env/rnvpce-page-index-ab-20261113/baseline-results/`
- owner 定向候选：`/home/zn/asterinas-build-env/rnvpce-owner-scheduling-20261114/results/`
- 最终状态机候选：`/home/zn/asterinas-build-env/rnvpce-owner-scheduling-20261114/state-machine-only/`

## 后续边界

本轮只减少 ready queue 的重复工作，没有改变 4 KiB dirty entry 的迁移粒度、worker
实际处理成本或设备提交路径。因此 32 线程剩余问题应继续从 worker 有效工作占比、
每轮 claim/完成页数和块层在途深度定位；不应再次采用简单 owner 定向唤醒。
