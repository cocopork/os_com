# 星绽 EXT4 4 KiB append 扩展性延迟分解（2026-11-24）

## 结论

星绽 EXT4 从 4 jobs 增加到 32 jobs 后的 append 回退，主要发生在前台
PageCache 容量 reservation 路径，不是后台 SSD I/O 或块分配锁。

低侵入采样直接测得，单次 4 KiB write 中，reservation 互斥锁的获取和释放等待
合计从 0.89 us 增长到 162.96 us。前台 PageCache/delalloc body 仅从 5.78 us
增长到 16.43 us。后台 writeback 每页总时间没有增长，块组和 superblock 发布锁等待
仍为亚微秒量级。

## 配置

- 设备：`/dev/nvme1n1p2`，每点重新格式化为 journaled EXT4；
- VM：64 vCPU、8 GiB，NVMe 8 virtqueue、8 IOThread；
- fio：4 KiB、sequential append、buffered、`psync`、`iodepth=1`；
- 每个 job 独占一个文件，总写入量 2 GiB；
- EXT4：`delalloc`，PageCache 上限 128 MiB；
- 对比：4 jobs 和 32 jobs，各一轮诊断；
- 前台高频指标按每 CPU 每 1024 次采样，避免全局统计原子本身成为串行点；
- allocator 由单一 writeback worker 调用，保留全量统计。

原始结果位于：

```text
/home/zn/asterinas-build-env/ext4-append-profile-20261124/sampled-results/
```

## 前台路径

| 阶段，us/4 KiB write | 4 jobs | 32 jobs | 增量 |
|---|---:|---:|---:|
| foreground total | 9.55 | 445.88 | +436.33 |
| PageCache/delalloc body | 5.78 | 16.43 | +10.65 |
| reservation acquire wait | 0.42 | 81.86 | +81.44 |
| reservation acquire hold | 0.24 | 4.42 | +4.18 |
| reservation release wait | 0.46 | 81.10 | +80.64 |
| reservation release hold | 0.11 | 0.15 | +0.05 |
| progress/trigger notify | 0.24 | 4.27 | +4.03 |

fio 观测值如下。诊断镜像仍有读时钟等插桩开销，因此绝对带宽不能与无插桩正式
矩阵直接混用；4/32 jobs 使用同一镜像，阶段增长可直接比较。

| jobs | 带宽 MiB/s | fio mean latency us |
|---:|---:|---:|
| 4 | 765.61 | 19.75 |
| 32 | 268.94 | 438.37 |

reservation 锁的双向等待是已测得的最大增长项。阶段之和仍不能覆盖全部
`foreground total`：高并发下线程在锁队列、唤醒和调度边界产生的 off-CPU 时间，
不会全部落入当前子阶段。该剩余项不能解释成块分配或设备 I/O 延迟。

## 后台 writeback

后台阶段采用第一轮全量计数，并按实际处理页数归一化。该轮高频前台探针会降低
吞吐，但后台只有一个 worker，不存在多线程争用同一统计项，适合判断阶段是否随
job 数恶化。

| 阶段，us/page | 4 jobs | 32 jobs | 变化 |
|---|---:|---:|---:|
| writeback round total | 8.00 | 6.29 | -1.71 |
| delalloc materialize | 0.52 | 0.66 | +0.15 |
| dirty scan | 0.02 | 0.04 | +0.02 |
| data submit | 1.71 | 1.97 | +0.26 |
| completion wait | 0.17 | 0.46 | +0.29 |
| metadata finish | 0.003 | 0.005 | +0.003 |

32 jobs 下 writeback round 的每页总时间没有上升。completion wait 虽约为 4 jobs
的 2.7 倍，但绝对增量仅约 0.29 us/page，无法解释前台数百微秒的增长。

## 块分配

| allocator 指标 | 4 jobs | 32 jobs |
|---|---:|---:|
| allocation calls | 8,313 | 9,355 |
| group wait，us/call | 0.11 | 0.23 |
| group hold，us/call | 5.48 | 14.33 |
| superblock publish wait，us/call | 0.11 | 0.21 |
| superblock publish hold，us/call | 0.58 | 1.37 |

32 jobs 产生了更多、更小的分配批次，group hold 也有所增加；但这些调用发生在单一
后台 writeback worker 上，按 524,288 个数据页摊销后增量远低于 1 us/page。
因此元数据分配路径存在可优化空间，但不是本次 32 jobs 性能断崖的主因。

## 实验边界

未追加 Linux 对照。星绽内部实验已经把主要增长定位到其全局 CachePressure
reservation 路径，而不是 QEMU、设备 I/O 或 allocator；Linux 对照不能进一步区分
这一已定位的内部串行点。

所有临时探针在实验完成后已从源码恢复，`pipeline_profile` 和 allocator metrics
保持关闭。

## Reservation 记账优化

针对上述全局锁瓶颈，先后验证了两种替代实现：

1. 单一共享 `AtomicUsize`：消除互斥锁睡眠，但 4 jobs 下共享缓存行和 CAS 重试造成
   明显回退，因此没有保留；
2. 8 个缓存行隔离的原子分区：当前 CPU 只更新一个分区，容量检查汇总 8 个分区，
   reservation 释放回原分区。跨分区更新和汇总使用顺序一致原子序，保证并发申请者
   不会同时漏计对方的预留量。

六种实现使用同一 NVMe、8 virtqueue、8 IOThread、128 MiB PageCache、4 KiB
buffered append 配置，各运行三轮。下表均为无插桩 release 镜像的 fio 计量期结果。

| 实现 | 4 jobs 带宽 MiB/s | 4 jobs mean us | 32 jobs 带宽 MiB/s | 32 jobs mean us |
|---|---:|---:|---:|---:|
| 原全局互斥锁 | 1059.10 | 13.39 | 295.83 | 395.62 |
| 单共享原子 CAS | 686.53 | 21.88 | 593.48 | 198.66 |
| 8 分区原子记账 | 1293.92 | 11.14 | 612.06 | 193.61 |
| 16 分区原子记账（否定） | 612.85 | 24.14 | 595.77 | 200.36 |
| 32 分区原子记账（否定） | 774.20 | 18.72 | 604.39 | 194.63 |
| per-CPU 32页批量额度（否定） | 497.72 | 29.98 | 460.66 | 262.60 |

8 分区方案相对原互斥锁：4 jobs 带宽提高 22.2%，32 jobs 带宽提高 106.9%；
32 jobs 平均延迟降低 51.1%。它既消除了全局锁在高并发下的排队，也避免了所有 CPU
反复修改同一原子缓存行。

进一步测试了与64个vCPU一一对应的本地额度：每个CPU批量领取32页，释放也批量归还。
该机制不适合当前持续高水位负载。PageCache填满后通常没有连续32页全局余量，批量领取
失败会触发对全部64个CPU闲置额度的回收扫描，使每次4 KiB写重新退化为
`O(CPU数)`。因此4 jobs和32 jobs分别比8分区方案下降61.5%和24.7%，该实现已撤销，
保留8分区原子记账。

增加普通原子分区数量同样无效。16和32分区虽然把32 jobs下每个分区的平均竞争者
从4个降至2个和1个，但每次reservation仍需读取全部分区。相对8分区，16分区的
32 jobs带宽下降2.7%，32分区下降1.3%；4 jobs分别下降52.6%和40.2%。说明8分区后
剩余写竞争已较小，继续增加跨缓存线汇总读取反而成为主要成本。

原始结果：

```text
/home/zn/asterinas-build-env/ext4-cache-reservation-atomic-20261124/mutex-control/
/home/zn/asterinas-build-env/ext4-cache-reservation-atomic-20261124/results/
/home/zn/asterinas-build-env/ext4-cache-reservation-sharded-20261124/results/
/home/zn/asterinas-build-env/ext4-cache-reservation-percpu-20261124/results/
/home/zn/asterinas-build-env/ext4-cache-reservation-shards16-20261124/results/
/home/zn/asterinas-build-env/ext4-cache-reservation-shards32-20261124/results/
```

六组正式对照的全部测试点均通过只读 `e2fsck -fn`。代码还通过
`cache_reservations_use_sharded_atomic_accounting` 定向测试、现有
`ext4_native_cache_pressure` 回归测试和带 `ktest` 的内核编译检查。

## 8 分区后的关键路径复测

为确认 8 分区是否真正消除了前台 reservation 瓶颈，使用同一 NVMe、8 virtqueue、
8 IOThread、128 MiB PageCache、2 GiB 数据量和 4 KiB buffered append 配置，对
4/32 jobs 各做一轮每 CPU 1/1024 低侵入采样。诊断镜像保持 8 分区实现不变。

### CachePressure reservation

| 阶段，us/call | 4 jobs | 32 jobs | 32-4 jobs |
|---|---:|---:|---:|
| 整次 claim（含下列子阶段） | 1.038 | 1.099 | +0.061 |
| 本地分区原子更新 | 0.108 | 0.110 | +0.002 |
| 汇总 8 个分区 | 0.096 | 0.105 | +0.008 |
| resident/dirty 压力检查 | 0.173 | 0.186 | +0.014 |
| 释放本地分区 | 0.081 | 0.098 | +0.016 |
| progress/trigger 通知 | 0.205 | 1.662 | +1.457 |

32 jobs 的通知均值受一个 370.8 us 离群样本影响；绝大多数样本仍低于 1 us。
即使直接采用均值，claim、释放和通知合计也只有约 2.86 us，无法解释前台约
200 us 的平均延迟。8 分区汇总本身仅约 0.105 us。因此原先
`CachePressure.reservations` 的全局互斥锁瓶颈已经消除。

容量不足后的真正等待仍会上升：按全部 524,288 次 4 KiB 写归一化，
`native_pressure_wait` 从约 1.28--2.31 us/write 增至 29.59--30.14 us/write。
这是后台释放容量速度不足的结果，不是 8 分区记账操作本身的锁竞争。

### 前台 write body

第二轮继续拆分 `write_delayed_chunk()`。低频采样下 fio 计量期带宽分别为
1071.13 MiB/s 和 602.18 MiB/s，趋势与无探针版本一致。

| 阶段，us/call | 4 jobs | 32 jobs | 32-4 jobs |
|---|---:|---:|---:|
| 前台 write 总计 | 16.88 | 217.32 | +200.44 |
| `write_native_chunk` body | 16.21 | 194.01 | +177.80 |
| 前置参数/状态检查 | 0.25 | 0.30 | +0.05 |
| inode 写锁获取 | 8.75 | 11.19 | +2.44 |
| ESTree mapping 检查 | 0.17 | 0.17 | 0.00 |
| `fs.reserve_delayed()` | 0.80 | 82.17 | +81.37 |
| PageCache resize | 0.08 | 0.10 | +0.01 |
| PageCache 数据写入 | 3.92 | 1.93 | -1.99 |
| 写后 delayed/元数据收尾 | 0.91 | 93.95 | +93.04 |

inode 锁均值包含少量毫秒级后台 writeback 冲突样本，但其增长只有 2.44 us；
PageCache 数据写入也没有随 jobs 增长。新的主要串行点是 delalloc 容量记账：

1. `fs.reserve_delayed()` 先查询 journal pinned-free，再由
   `DelayedAccount::reserve()` 获取全文件系统 `block_capacity.state` 互斥锁，更新
   `owners` 和 `delayed`；
2. 每次写后的 `retain_delayed_promise()` 先调用 `remaining()`，再调用 `retain()`，
   会对同一把全局 `block_capacity.state` 锁再获取两次；
3. 因而每个 4 KiB append 至少三次竞争这套全局容量状态。该路径不属于已经分区的
   `CachePressure.reservations`。

后台也观察到相同方向：delalloc materialize 从约 0.37 us/page 增至
2.32 us/page，writeback round 从约 3.60 us/page 增至 6.53 us/page。后台增量会降低
容量释放速度，并进一步放大前台 `pressure_wait`。

结论：8 分区方案已经解决原 CachePressure reservation 多线程瓶颈；当前首要瓶颈
转移到 `CapacityState` 的全局 delayed-allocation 记账，次生表现为容量等待。下一步
应合并每次写中的 reserve/remaining/retain，或把 delayed promise 改成 inode/CPU
本地增量后批量汇总；不应继续增加 CachePressure reservation 分区数。

原始数据：

```text
/home/zn/asterinas-build-env/ext4-reservation-8shard-profile-20261124/results/
/home/zn/asterinas-build-env/ext4-reservation-8shard-profile-20261124/body-results/
```

两轮均通过只读 `e2fsck -fn`。临时探针在镜像生成后已从源码撤销，
`pipeline_profile::ENABLED` 已恢复为 `false`。

## Delayed promise 单锁收尾优化

进一步尝试消除 `CapacityState` 重复获取。第一版使用 RAII token，让完整写提交时跳过
收尾锁，但 token 每个 4 KiB 写都引入额外所有权管理；同时段对照中 4/32 jobs 分别
低约 6%/5%，因此该方案已完整撤销。

最终保留的方案严格维持原语义：新增 `DelayedAccount::retain_at_most()`，在一次
`CapacityState` 临界区内完成原先 `remaining()` 查询、`min(pending, remaining)` 和
`retain()` 退款。每次前台 append 的全局容量锁由三次降为两次，不改变 reserve、
ENOSPC、短写退款或后台 materialize 协议。

由于本轮宿主负载存在明显波动，使用同一时段、同一镜像参数各三轮对照：

| 实现 | 4 jobs MiB/s | 32 jobs MiB/s |
|---|---:|---:|
| 原 `remaining()+retain()` | 553.42 | 481.40 |
| 单锁 `retain_at_most()` | 695.98 | 619.49 |
| 提升 | +25.8% | +28.7% |

低侵入分解进一步确认改善来自目标路径：

| 阶段，us/call | 优化前 32 jobs | 优化后 32 jobs | 变化 |
|---|---:|---:|---:|
| 前台 write 总计 | 217.32 | 139.46 | -35.8% |
| `write_native_chunk` body | 194.01 | 119.79 | -38.3% |
| `fs.reserve_delayed()` | 82.17 | 57.58 | -29.9% |
| delayed/元数据收尾 | 93.95 | 51.36 | -45.3% |

收尾阶段下降后，`reserve_delayed()` 也因全局锁排队减少而同步下降。当前剩余首要热点
仍是每次新增 delayed block 必须执行一次的 `DelayedAccount::reserve()`；若继续优化，
需要设计可回收的 inode/CPU 本地 promise 批次，不能简单预留固定额度而在高水位
长期囤积。

原始数据：

```text
/home/zn/asterinas-build-env/ext4-delayed-promise-delta-20261124/control-results/
/home/zn/asterinas-build-env/ext4-delayed-retain-once-20261124/results/
/home/zn/asterinas-build-env/ext4-delayed-retain-once-20261124/profile-results/
```

全部正式测试点均通过只读 `e2fsck -fn`。临时分解探针已撤销。

### 可靠性复测

由于上一轮 4 jobs 三次结果在 423--851 MiB/s 间波动，不能直接使用三轮均值，
重新进行了两组反向顺序测试和一组闭环测试。当前优化镜像共取得 9 个样本；同时在
相同设备状态下复测优化前的 8 分区镜像 3 次。两者均使用 8 virtqueue、8 IOThread、
128 MiB PageCache、2 GiB 数据量和 4 KiB buffered append。

| 镜像 | jobs | 样本数 | 均值 MiB/s | 中位数 MiB/s | 范围 MiB/s | 变异系数 |
|---|---:|---:|---:|---:|---:|---:|
| 单锁 `retain_at_most()` | 4 | 9 | 599.27 | 528.11 | 493.49--873.72 | 23.9% |
| 原 `remaining()+retain()` | 4 | 3 | 467.40 | 476.94 | 437.05--488.20 | 5.8% |
| 单锁 `retain_at_most()` | 32 | 9 | 667.35 | 652.02 | 613.54--736.16 | 8.0% |
| 原 `remaining()+retain()` | 32 | 3 | 542.94 | 546.28 | 516.78--565.75 | 4.5% |

32 jobs 下优化镜像的均值和中位数分别高 22.9% 和 19.4%，方向稳定。4 jobs 下存在
两个 819/874 MiB/s 高值，均值不可靠；采用中位数比较，优化约 10.7%。因此可以确认
合并全局锁获取有效，但不能沿用上一轮报告的 25.8%/28.7% 作为精确收益。

历史 8 分区镜像曾测得 4 jobs 平均 1293.92 MiB/s，但同一旧镜像在本次复测中只有
467.40 MiB/s。说明绝对带宽还受到未被当前脚本控制的设备或宿主状态影响；历史高值
不能作为当前代码回退的证据。后续比较应采用同一时段交错 A/B，并同时报告中位数、
范围和变异系数。

复测的 24 个文件系统检查状态均为 0。原始数据：

```text
/home/zn/asterinas-build-env/ext4-delayed-retain-once-retest-20261124/batch-a/
/home/zn/asterinas-build-env/ext4-delayed-retain-once-retest-20261124/batch-b/
/home/zn/asterinas-build-env/ext4-delayed-retain-once-retest-20261124/batch-c/
/home/zn/asterinas-build-env/ext4-delayed-retain-once-retest-20261124/old-control/
```
