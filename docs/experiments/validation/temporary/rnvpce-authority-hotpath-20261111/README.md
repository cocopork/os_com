# rNVPCe 前台权威记录热路径优化与验证

日期：2026-11-11

## 结论

本轮消除了覆盖写路径中重复查询和读取 inode 权威记录、逐次创建 publication
上下文、以及无等待者时仍执行 metadata pin 全局广播的开销。4 KiB 顺序覆盖写
的三轮无插桩均值如下：

| fio 线程 | 优化前 MiB/s | 优化后 MiB/s | 带宽变化 | 优化前平均延迟 | 优化后平均延迟 | 延迟变化 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 512.31 | 911.30 | +77.9% | 30.13 us | 16.66 us | -44.7% |
| 16 | 607.90 | 735.41 | +21.0% | 100.53 us | 83.60 us | -16.8% |
| 32 | 384.47 | 413.12 | +7.5% | 308.41 us | 286.38 us | -7.1% |

4 和 16 线程性能显著提升，证明原权威记录热路径是实际串行热点。32 线程仍然
回退，说明本轮只解决了已定位瓶颈，并未消除所有高并发共享开销。

## 实现

- `Inode` 缓存固定 inode 权威记录的稳定 `PersistentObjectRef`，每次写仍重新读取
  `InodeRecord`，不跨写缓存 generation 等可变状态。
- 单次前台写只读取、校验一次 `InodeRecord`，随后在 range plan、generation 检查
  和 commit 中复用同一快照；commit 返回最终 `block_count`，不再事后重复读取。
- 引用和记录封装为只能由校验读取路径生成的 `LoadedFixedInode`，避免后续调用者
  向 commit 传入不匹配的引用与记录；公开 `fixed_inode_ref` 保留原有内容和 owner 校验。
- `Inode` 持有 publication coordinator，在 inode 生命周期内复用；每次事务仍单独
  获取 publication guard。
- metadata pin 状态变化仅在确有等待者时更新 epoch 并 `wake_all`。慢路径先登记
  waiter，再重查 epoch 和阻塞条件，避免检查与睡眠之间丢失唤醒。

## 无插桩性能

测试条件：真实设备 `/dev/nvme1n1p2`，4 KiB、顺序覆盖写、2 GiB 总数据量、
128 MiB rNVPC 池、4 个 rTIO worker、4 个 virtqueue。每个测试点先建立低碎片连续
文件；4、16、32 线程各运行三轮。

| 线程 | 第 1 轮 | 第 2 轮 | 第 3 轮 | 均值 | 最小值 | 最大值 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 932.18 | 893.93 | 907.80 | 911.30 | 893.93 | 932.18 |
| 16 | 745.27 | 686.10 | 774.88 | 735.41 | 686.10 | 774.88 |
| 32 | 399.69 | 432.52 | 407.16 | 413.12 | 399.69 | 432.52 |

表中带宽单位均为 MiB/s。所有点完成 2 GiB 写入、sync、卸载和后置 e2fsck。

优化前数据采用同一 overwrite sweep 的五轮结果，路径为
`/home/zn/asterinas-build-env/ext4-rnvpce-overwrite-sweep-20261109-5x/`。

## 阶段诊断

诊断镜像临时启用 `pipeline_profile`；构建后立即恢复开关。探针会改变调度和吞吐，
所以下表只用于比较阶段，不与无插桩带宽混用。

| 阶段，us/4 KiB | 4 线程 | 16 线程 | 32 线程 |
| --- | ---: | ---: | ---: |
| foreground total | 26.40 | 72.34 | 241.65 |
| pre guard | 0.58 | 1.71 | 2.60 |
| publication acquire | 0.58 | 1.68 | 1.07 |
| guarded authority | 0.88 | 2.10 | 1.36 |
| plan inode | 0.00 | 0.00 | 0.00 |
| plan total | 4.44 | 13.46 | 12.01 |
| span prepare | 3.80 | 10.93 | 9.17 |
| data reserve | 1.32 | 2.64 | 3.66 |
| post commit | 3.71 | 11.22 | 97.33 |
| persist | 8.66 | 19.43 | 110.25 |

`plan_inode` 调用数为 0，证明前台已不再由 plan 重读 inode 权威记录。
`guarded_authority` 也不再随线程数近似线性增大。32 线程的新主要延迟位于
`post_commit` 和 `persist`，后续应继续拆分 NVPage 提交及持久化路径。

## 正确性验证

- `cargo osdk check --ktests --package aster-kernel`：通过，仅有既存警告。
- `rnvpce_cached_inode_authority_reads_record_once`：通过。
- `rnvpce_range_plan_reuses_loaded_inode_authority`：通过。
- `rnvpce_extent_node_writer_intent_blocks_new_readers`：通过。
- 三轮性能点的后置 e2fsck：全部通过。

新增测试分别验证稳定引用只解码一次记录、无竞争 metadata read 不广播 pin 状态、
range plan 复用已加载的 inode 记录，以及原有 writer intent 并发语义未回归。

## 原始证据

- 无插桩镜像及输入：
  `/home/zn/asterinas-build-env/rnvpce-authority-hotpath-20261111/source/`
- 三轮无插桩结果：
  `/home/zn/asterinas-build-env/rnvpce-authority-hotpath-20261111/round-{1,2,3}/`
- 诊断镜像及输入：
  `/home/zn/asterinas-build-env/rnvpce-authority-hotpath-20261111/diagnostic-source/`
- 阶段诊断结果：
  `/home/zn/asterinas-build-env/rnvpce-authority-hotpath-20261111/diagnostic/`

最终源码保持 `pipeline_profile::ENABLED = false`。

## 32 线程热点 A/B 验证

针对 32 线程下 `post commit` 和 `persist` 的突增，分别进行了两个单变量实验。
实验代码在测量后均已回退，不属于最终实现。

### A：取消 fast 模拟器全局持久化计数

仅在非 ktest 的 `rnvpc_sim_mode=fast` 后端跳过 `persist_calls` 和
`fence_calls` 全局原子计数，保留全部持久化调用边界、编译器 fence 和上层协议。

| 阶段，us/4 KiB | 原始 4 线程 | A 4 线程 | 原始 16 线程 | A 16 线程 | 原始 32 线程 | A 32 线程 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| foreground total | 26.40 | 18.77 | 72.34 | 72.47 | 241.65 | 231.53 |
| post commit | 3.71 | 2.86 | 11.22 | 13.13 | 97.33 | 78.14 |
| persist | 8.66 | 5.79 | 19.43 | 18.11 | 110.25 | 106.00 |

32 线程 fio 带宽从 488.08 MiB/s 变为 496.12 MiB/s，仅提高 1.6%。全局统计
原子存在可测开销，但不是 `persist` 增长到约 110 us 的主要原因。

### B：按 inode owner 定向通知并合并重复唤醒

恢复原持久化计数，只将 inode 写后通知改为唤醒所属 rTIO worker，并用每 worker
pending 位合并尚未处理的重复通知。插桩单轮结果为：

| 阶段，us/4 KiB | 原始 4 线程 | B 4 线程 | 原始 16 线程 | B 16 线程 | 原始 32 线程 | B 32 线程 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| foreground total | 26.40 | 24.99 | 72.34 | 73.62 | 241.65 | 215.03 |
| post commit | 3.71 | 3.11 | 11.22 | 13.82 | 97.33 | 80.98 |
| persist | 8.66 | 6.50 | 19.43 | 20.65 | 110.25 | 96.07 |

插桩下 32 线程带宽从 488.08 MiB/s 变为 516.52 MiB/s，说明广播和重复通知会
放大前台竞争；但三轮无插桩复测出现回退：

| 线程 | 原实现 MiB/s | B 三轮 MiB/s | B 均值 MiB/s | 原实现平均延迟 | B 平均延迟 |
| ---: | ---: | --- | ---: | ---: | ---: |
| 4 | 911.30 | 608.62 / 645.24 / 681.30 | 645.05 | 16.66 us | 22.03 us |
| 16 | 735.41 | 726.24 / 657.89 / 615.94 | 666.69 | 83.60 us | 92.02 us |
| 32 | 413.12 | 394.07 / 381.88 / 430.52 | 402.16 | 286.38 us | 297.25 us |

pending 合并降低了通知成本，也降低了迁移推进频率，容量压力抵消并超过前台收益。
因此，32 线程断崖并非“优化只做到 16 线程”，也不能通过简单地减少唤醒解决。
后续需要拆分 `persist` 中 redirect entry 扩展/新建、entry 状态冲突和迁移并发造成的
事务退化次数，并把迁移调度改成有阈值的 owner 定向批处理，而不是每页广播或无条件
合并。

原始证据位于：

- `/home/zn/asterinas-build-env/rnvpce-32t-ab-20261111/persist-no-counter/`
- `/home/zn/asterinas-build-env/rnvpce-32t-ab-20261111/owner-coalesced/`
- `/home/zn/asterinas-build-env/rnvpce-32t-ab-20261111/owner-coalesced-release-round-{1,2,3}/`

## 32 线程持久化路径继续拆分（2026-11-12）

为验证 32 线程延迟突增是否来自“可扩展性优化只覆盖到 16 线程”，对 fast-path
redirect entry 扩展进行了逐层临时插桩。所有新增探针和 A/B 代码在实验后均已撤销。

### 调用形态

每个测试点均执行 524,288 次 4 KiB 写。entry 原地扩展成功次数为 4/16/32
线程下约 518k/512k/509k，即约 97% 的写仍走扩展快路径。32 线程没有出现大量
`EBUSY`、回退到新建 entry 或事务重试，因此不是快路径在 16 线程后失效。

| 阶段，us/4 KiB | 4 线程 | 16 线程 | 32 线程 |
| --- | ---: | ---: | ---: |
| commit prepare | 0.27 | 1.52 | 3.03 |
| fast validate | 0.27 | 1.52 | 3.05 |
| mutation begin | 0.27 | 1.52 | 3.03 |
| data stage | 0.98 | 2.70 | 4.16 |
| root handling | 0.99 | 2.30 | 3.95 |
| redirect entry extend | 5.06 | 8.63 | 22.08 |
| publish fallback | 0.59 | 6.25 | 3.20 |
| activate | 0.29 | 1.54 | 3.24 |

entry 扩展是 32 线程下最大的单项，继续拆分后，NVM 持久化本身只有约
2.70 us；主要墙钟时间位于扩展完成阶段。再拆到 redirect index 内部，结果为：

| redirect index 完成阶段，us/4 KiB | 4 线程 | 16 线程 | 32 线程 |
| --- | ---: | ---: | ---: |
| inode map 查找 | 0.63 | 2.03 | 3.56 |
| entry 状态提交 | 0.38 | 1.76 | 3.12 |
| migration worker 集合查找 | 4.75 | 7.33 | 15.81 |
| worker dirty-page 记账 | 0.58 | 1.88 | 3.31 |
| index finish 总计 | 8.55 | 21.85 | 41.68 |

这些数值是包含线程失去 CPU 时间的墙钟延迟，不能直接解释为锁持有时间。诊断器
自身还会为每个阶段更新 count、total、max、units 和 histogram 五组全局原子量，
高线程下会放大 cache-line 竞争，因此只用于定位，不能当作 release 性能。

### 全局 worker 表锁 A/B

实验性地将 `MigrationWorkerSet` 引用缓存到 per-inode redirect map，保留原有
dirty-page 记账和唤醒语义，只消除每页获取全局 `migration_workers` 表锁。

| 插桩指标，us/4 KiB | 原始 4T | 缓存 4T | 原始 16T | 缓存 16T | 原始 32T | 缓存 32T |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| worker lookup | 4.75 | 3.88 | 7.33 | 6.14 | 15.81 | 13.89 |
| redirect extend finish | 10.35 | 10.77 | 28.96 | 26.14 | 54.41 | 51.25 |
| foreground persist | 23.56 | 26.44 | 76.62 | 70.71 | 137.44 | 132.30 |
| foreground total | 42.36 | 47.47 | 134.85 | 124.07 | 243.01 | 233.60 |

移除全部临时探针后，在同一当前源码上做单轮严格对照：

| 线程 | 原实现 MiB/s | per-inode 缓存 MiB/s | 变化 |
| ---: | ---: | ---: | ---: |
| 4 | 309.93 | 330.11 | +6.5% |
| 16 | 402.36 | 458.06 | +13.8% |
| 32 | 433.62 | 409.52 | -5.6% |

该 A/B 没有改善 32 线程点，故全局 worker 表锁不是 32 线程断崖的主因，缓存改动
未保留。源码中 redirect index 使用 64 个分区；32 线程测试为 fio 分配 user CPU
0–31，rTIO flush CPU 和 virtio CPU 另行隔离，也不存在显式的 16 线程容量上限。

当前干净基线的总吞吐在 16–32 线程间只从 402.36 增至 433.62 MiB/s，已接近该
版本软件管线的吞吐平台。并发数翻倍而吞吐仅增加 7.8% 时，单次同步写延迟按排队
规律必然接近翻倍；这与本轮看到的多项短阶段同时增长一致，而不像某一个仅支持
16 线程的锁或数组边界。下一步应使用 per-CPU 统计或采样 profiler 区分实际运行
时间与被抢占/排队时间，再定位约 430 MiB/s 的共同吞吐上限。

原始证据：

- 逐层插桩：`/home/zn/asterinas-build-env/rnvpce-persist-breakdown-20261112/{results,extend-results,finish-results,index-results}/`
- per-inode 缓存插桩 A/B：`/home/zn/asterinas-build-env/rnvpce-persist-breakdown-20261112/inode-worker-cache-results/`
- per-inode 缓存无探针对照：`/home/zn/asterinas-build-env/rnvpce-persist-breakdown-20261112/inode-worker-cache-clean-results/`
- 当前源码无探针基线：`/home/zn/asterinas-build-env/rnvpce-persist-breakdown-20261112/current-clean-baseline-results/`

## redirect 扩展记账与写后通知优化（2026-11-12）

本轮按计划先处理 redirect entry 扩展记账，再处理 post-commit 通知；write plan
路由缓存明确留待后续评估，并且若实施将扩展既有 extent status tree，而不另建一套缓存。

### 实现结果

- 将 inode 的 queued-dirty-page 计数和 ready-queue 边沿状态移到原子字段。已有
  redirect entry 的扩展只更新 inode/worker 两级计数并在空到非空边沿入队，不再
  获取 inode migration 锁；owner、generation 和迁移阶段计数仍由原锁保护。
- 每个 rTIO worker 增加 pending 位。连续 post-commit 请求在 worker 确认前只推进
  一次 epoch、产生一次有效唤醒；worker 先采样 epoch 再确认 pending 边沿，避免
  通知与确认交错时丢失下一轮工作。
- A/B 证明 inode-owner 定向唤醒会放大稳定 owner 的负载不均，降低在途窗口，因此
  最终保留全 worker 通知语义，只合并每个 worker 上尚未确认的重复通知。

### 无插桩性能

测试条件与当前干净基线相同：真实设备 `/dev/nvme1n1p2`、4 KiB 顺序覆盖写、
2 GiB 数据、128 MiB rNVPC 池、4 个 rTIO worker。当前干净基线每点只有一轮，
最终候选每点三轮，因此百分比只用于判断方向。

| 线程 | 当前干净基线 | 候选三轮 MiB/s | 候选均值 | 相对变化 |
| ---: | ---: | --- | ---: | ---: |
| 4 | 309.93 | 367.16 / 491.36 / 317.27 | 391.93 | +26.5% |
| 16 | 402.36 | 438.17 / 454.40 / 445.22 | 445.93 | +10.8% |
| 32 | 433.62 | 442.24 / 440.05 / 424.02 | 435.44 | +0.4% |

通知确认顺序修正后的 32 线程三轮为 `385.18 / 440.05 / 429.98 MiB/s`；首轮是
明显低值，后两轮回到候选区间。综合全部数据，16 线程收益明确，32 线程没有稳定
回退，但也没有突破约 430 MiB/s 的现有管线平台。4 线程存在较大运行波动。

### A/B 诊断

32 线程下，仅做 redirect 记账优化得到 `431.61 MiB/s`。加入 owner 定向通知与
合并后降至 `406.03 MiB/s`；保留定向通知但取消合并为 `420.62 MiB/s`。对应 worker
提交页数一度失衡为约 `98K/82K/180K/164K`，最大在途页从 redirect-only 的
`11243` 降至 `2117`。因此最终实现撤销定向通知，采用广播语义下的逐 worker
边沿合并。

### 验证与证据

- `cargo fmt --check`、`git diff --check`：通过。
- `cargo osdk check --ktests --package aster-kernel`：通过，仅有既存警告。
- `rnvpce_extension_accounting_stays_on_atomic_fast_path`：QEMU ktest 通过。
- `rnvpce_migration_worker_notification_is_edge_triggered`：QEMU ktest 通过。
- `rnvpce_abort_requeue_serializes_with_owner_turnover`：旧实现 QEMU ktest 失败；
  abort/requeue 慢路径恢复 migration 锁后通过。该锁关闭旧 owner 释放与 ready
  重新入队之间的 ABA 窗口，不影响常见 entry 扩展路径。
- 所有 fio 点均完成 sync、卸载和后置 e2fsck。

独立并发审查未发现其他生产代码缺陷，但现有测试没有用强制调度逐一覆盖
notification/acknowledgement 和 dequeue/extension 的所有交错；上述结论同时依赖
锁序审查、原子内存序分析、QEMU 功能测试与真实设备压力运行。

原始结果：

- 第一项单独结果：`/home/zn/asterinas-build-env/rnvpce-foreground-scaling-20261112/task1-results/`
- 中间 A/B：`task2-combined-results/`、`task2-owner-only-results/`、
  `task2-broadcast-coalesced-results/`，均位于上述 `rnvpce-foreground-scaling-20261112/`。
- 最终三轮：`/home/zn/asterinas-build-env/rnvpce-foreground-scaling-20261112/final-candidate-r{1,2,3}-t{4,16,32}/`
- 通知边界修正后 32T：同目录下 `final-racefix-t32/` 和
  `final-racefix-r{2,3}-t32/`。

## EXT4 EST write plan 路由复用（2026-11-12）

本轮按后续计划实施第三项优化，但没有新建独立 inode 路由缓存。现有 EXT4
`EsCache` 内新增独立的 rNVPCe SSD-route 命名空间，仍复用同一把 EST 内部锁；原生
EXT4 mapping fact 与 NVM 权威树事实不混用。缓存只记录完整的
`logical range -> SSD range + written state`，不缓存 hole，并按 NVM inode generation
整体失效。

前台 write plan 先查询 redirect index。redirect 完整覆盖时直接跳过 SSD extent
路由；否则只在 EST 对请求范围具有完整覆盖时命中。miss 仍走原 NVM extent tree，
但会把已经验证的完整 extent 回填 EST，而不是只缓存本次 4 KiB 裁剪片段。分配写、
hole 和 allocation predecessor 的原语义不变。

定向 ktest 证明：缓存提供的 SSD route 和 redirect 完整覆盖两种路径均产生 0 次
NVM extent-tree range lookup；普通 cache miss 仍保留 `NewAllocation` 和 predecessor
goal。EST 回归覆盖完整命中、部分覆盖 miss、generation 切换、物理不连续、written
状态、重叠替换和容量淘汰。

真实设备测试仍使用 `/dev/nvme1n1p2`、4 KiB 顺序覆盖写、2 GiB 数据、128 MiB
rNVPC 池和 4 个 rTIO worker。每点三轮，表中基线是上一节最终候选三轮均值。

| 线程 | EST 三轮 MiB/s | EST 均值 | EST 中位数 | 前一候选均值 | 均值变化 |
| ---: | --- | ---: | ---: | ---: | ---: |
| 4 | 744.73 / 715.08 / 803.45 | 754.42 | 744.73 | 391.93 | +92.5% |
| 16 | 459.81 / 524.72 / 765.04 | 583.19 | 524.72 | 445.93 | +30.8% |
| 32 | 404.74 / 407.00 / 378.84 | 396.86 | 404.74 | 435.44 | -8.9% |

4、16 线程的 write plan 收益明确，尤其是低并发下反复读取 NVM extent leaf 的成本
已经消除。32 线程仍位于约 400 MiB/s 的共享管线平台，且本轮相对前一候选存在小幅
回退；因此这项修改解决的是路由读取，不代表 32 线程串行瓶颈已经解决。三轮 16
线程波动也较大，后续比较应继续报告逐轮值，不能只用单轮峰值。

所有 9 个有效 fio 点均完成 sync、卸载和只读 e2fsck，`e2fsck.status=0`。release
ISO SHA-256 为
`081c265b469c0efd4b8d2b76e5a72836d5634197c72bdaa60f58c182cda1b984`。
首个 `round-1/` 因误用不含 fio 的精简 initramfs 在准备阶段退出，未进入性能统计；
有效数据只来自：

- `/home/zn/asterinas-build-env/rnvpce-est-route-cache-20261112/round-1-valid/`
- `/home/zn/asterinas-build-env/rnvpce-est-route-cache-20261112/round-2-valid/`
- `/home/zn/asterinas-build-env/rnvpce-est-route-cache-20261112/round-3-valid/`

### 收尾审查与无分配命中路径

独立审查没有发现 stale route、hole 误判或 authority 生命周期缺陷，但发现首版
rNVPCe route map 使用 `BTreeMap`，更新时会在 EST spinlock 内分配或释放节点；4 KiB
单 extent 命中还会连续构造 route `Vec` 和 planner `Vec`。最终实现将 rNVPCe route
层改为首次使用时在锁外一次性预留 512 项容量的有序 `Vec`，锁内只做已有容量内的
retain/insert；单 extent 覆盖查询返回一个值，并通过单 route planner 入口避免中间
两个堆分配。多 route 查询仍在锁外预留结果容量。

新增端到端 ktest 使用真实 NVM walker 发现的完整 extent 作为下一次规划输入，交叉
验证 logical range、SSD start 和 written 状态，并确认第二次规划树读取为 0。

无分配版本 32 线程两轮为 `371.49 / 394.68 MiB/s`，处于首版 EST 三轮
`378.84–407.00 MiB/s` 的波动范围，没有证据表明这两个小型分配是约 400 MiB/s
平台的主因。该修改因满足锁内不分配约束而保留，但不宣称解决了 32 线程瓶颈。
对应原始结果为：

- `/home/zn/asterinas-build-env/rnvpce-est-route-cache-20261112/noalloc-r1-t32/`
- `/home/zn/asterinas-build-env/rnvpce-est-route-cache-20261112/noalloc-r2-t32/`
