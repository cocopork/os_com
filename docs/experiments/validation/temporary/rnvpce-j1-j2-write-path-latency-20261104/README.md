# rNVPCe 4 KiB 覆盖写 1/2 线程延迟分解（2026-11-04）

## 结论

release 测试中，fio 平均延迟从 1 线程的 18.36 us 增加到 2 线程的
132.22 us。诊断镜像复现为 18.39 us 和 140.00 us。新增的细分探针表明，
差值不是容量等待、publication guard 等待、SSD 分配、rTIO flush 或 NVDIMM
persist 引起的；主要原因是前台每次 4 KiB overwrite 对 SSD baseline extent tree
执行了两次路由，而 2 线程准备出的文件高度碎片化，使两次路由分别增加
58.75 us 和 64.09 us。

第一次路由位于 `plan_range_write -> lookup_tree_extent_ranges`。第二次路由位于
`allocation_goal -> current_page_mapping`。覆盖写中 `newly_reserved == 0`，不会分配
SSD block，因此第二次查询的结果没有消费者，是可以直接消除的冗余工作。

## 测试条件

- 设备：`/dev/nvme1n1p2`，每轮重新格式化为 4 KiB、nojournal EXT4；
- 来宾：64 vCPU、8 GiB、4 个 virtqueue，rNVPCe Native + rTIO；
- NVM：128 MiB pool，64 MiB metadata reserve，64 MiB data limit，fast 模式；
- workload：2 GiB、4 KiB、buffered `psync`、随机覆盖写；
- 对照：1 job 对一个 2 GiB 文件，2 jobs 各对一个 1 GiB 文件；
- 测量前先原生 direct 128 KiB 写入文件，再执行一轮 rNVPCe warmup；
- pipeline 计数在 warmup 后和被测 workload 后各 dump 一次，表中为两次快照差值；
- 诊断 ISO SHA-256：`b68bae505f226b66d32d06c84dd0fb35c77c102d3c6155744c6b4e5125008399`。

## 端到端结果

| jobs | fio 带宽 (MiB/s) | fio 平均延迟 (us) | fio p99 (us) | sync/umount | e2fsck |
|---:|---:|---:|---:|---:|---:|
| 1 | 207.75 | 18.39 | 36.10 | 0 / 0 | 0 |
| 2 | 54.73 | 140.00 | 156.67 | 0 / 0 | 0 |

诊断探针有固定开销，因此绝对值以 release 的 18.36/132.22 us 为准；诊断运行
用于解释差值。两组均精确完成 2 GiB，fio、sync、umount 和运行后 e2fsck 均成功。

## 前台路径分解

单位为每个成功写入的 4 KiB page 的平均时间。每组测量 524,288 pages。

| 阶段 | 1 job (us/page) | 2 jobs (us/page) | 差值 (us/page) |
|---|---:|---:|---:|
| 前台总计 | 17.711 | 138.830 | +121.119 |
| pre-guard | 0.678 | 0.732 | +0.054 |
| publication guard 获取 | 0.209 | 0.278 | +0.069 |
| guarded authority | 0.586 | 0.592 | +0.006 |
| write plan 总计 | 2.947 | 61.448 | **+58.501** |
| └ inode record | 0.569 | 0.568 | -0.001 |
| └ redirect index | 0.894 | 0.335 | -0.559 |
| └ SSD extent tree | 0.960 | 59.713 | **+58.753** |
| └ plan build | 0.080 | 0.083 | +0.003 |
| span prepare 总计 | 2.401 | 66.813 | **+64.412** |
| └ NVPage reserve | 0.456 | 0.453 | -0.003 |
| └ allocation goal 查询 | 1.248 | 65.334 | **+64.086** |
| └ request build | 0.095 | 0.097 | +0.002 |
| SSD allocate | 0.053 | 0.059 | +0.006 |
| commit/persist | 8.074 | 7.044 | -1.030 |
| post-commit | 0.803 | 0.672 | -0.131 |

两个 extent-tree 路径合计解释了全部观测差值；其他阶段没有出现随线程数增长的
显著锁等待。既有详细锁探针也显示 publication wait 约 0.18 us/page、inode
write-inner wait 约 0.07 us/page，1/2 线程基本相同。

## 树复杂度证据

`lookup_tree_extent_ranges` 每读取一个持久 extent node 都要读取、复制并校验一个
4 KiB `ExtentNodeRecord`。测得：

| jobs | tree node reads / page | tree read (us/page) | tree scan/validate (us/page) |
|---:|---:|---:|---:|
| 1 | 1.000 | 0.520 | 0.440 |
| 2 | 2.933 | 29.842 | 29.871 |

运行后用 debugfs 检查同一 SSD baseline：

| jobs | 文件 | data extents | extent index blocks |
|---:|---|---:|---:|
| 1 | 2 GiB `f.0` | 17 | 1 |
| 2 | 1 GiB `f.0` | 6,669 | 21 |
| 2 | 1 GiB `f.1` | 6,667 | 21 |

这是直接证据：两线程原生准备阶段产生了大量约 32-page 的交错 extent，rNVPCe
随后导入并遍历复杂的 SSD baseline tree。具体由 EXT4 分配器如何交错两个 inode
属于下一层原因，尚未用分配器探针验证；但“复杂树 + 每页两次路由”已经由调用
次数、阶段时间和磁盘布局三类证据相互印证。

## 代码对应关系

1. 前台先调用 `plan_range_write`；该函数读取 inode、redirect index 和 SSD extent
   tree。树查询会逐节点读取完整的 4 KiB record，并扫描 entry。
2. 建好 spans 后，无条件调用 `current_page_mapping(logical_block - 1)` 计算
   allocation goal；该函数再次从 inode root 路由 extent tree。
3. 覆盖写的 `newly_reserved` 为 0，但代码仍执行第二次查询，再调用零长度 SSD
   allocation。
4. fast DRAM backend 的普通 `read_page` 没有全局锁；`io_lock` 仅编译到 ktest
   crash-model 路径，因此不能把本次延迟解释为 NVM 后端全局锁竞争。

## 优化顺序

1. **立即优化**：仅在 `newly_reserved > 0` 时计算 allocation goal。覆盖写可直接
   消除第二次完整路由，按本次 2-job 数据预计可减少约 65 us/page。
2. **复用查询结果**：首次写确实需要 allocation goal 时，由 `plan_range_write`
   一并返回前驱 SSD block，禁止再次从 root 查询。
3. **消除 4 KiB overwrite 的持久树全路由**：为 SSD baseline 建 inode-local
   DRAM range index/leaf cache，以 generation 或结构修改序号失效；NVM tree 仍是
   权威持久结构。该项针对剩余约 60 us/page。
4. **单独处理分配碎片**：检查两 inode 并发首次分配为何形成约 6.7k extents；
   这是 SSD allocator/locality 问题，不应与覆盖写查询优化混为一项。

## 证据目录与限制

- 1-job：`/home/zn/asterinas-build-env/rnvpce-flusher-batching-20261104/deep-j1`；
- 2-job：`/home/zn/asterinas-build-env/rnvpce-flusher-batching-20261104/deep2-j2`；
- 1-job 磁盘布局复核：同目录根下 `deep3-j1`；
- 错误准备成 j1 的 `deep-j2` 已保留但明确排除，不参与任何表格或结论。

运行封装器在 workload 和 e2fsck 完成后，因宿主 NVMe 多 hctx 分布门槛返回 3。
该门槛用于 virtio 多队列驱动验收；本轮研究的是来宾前台 write 路径，且三轮的
fio/sync/umount/e2fsck 均为 0，因此不影响阶段时间结论。报告不据此声称本轮完成
了多 hctx 驱动验收。

## allocation goal 优化验证

实现做了三项调整：覆盖写不再计算 allocation goal；首次分配直接复用
`plan_range_write` 已读取叶节点中的最近 SSD 前驱；没有树前驱时使用 inode-local
分配 hint，并在成功分配后更新。hint 仅决定分配器的搜索起点，实际空闲状态仍由
block-group bitmap 校验，因此陈旧 hint 不影响正确性。

关闭全部延迟探针的 release 结果如下。优化前数据来自本报告原始基线，优化后
1-job 重复两次，2-job 测试一次：

| jobs | 版本 | fio 带宽 (MiB/s) | fio 平均延迟 (us) | fio p99 (us) |
|---:|---|---:|---:|---:|
| 1 | 优化前 | 208.68 | 18.36 | 24.70 |
| 1 | 优化后，第 1 次 | 186.49 | 20.54 | 24.70 |
| 1 | 优化后，第 2 次 | 188.77 | 20.28 | 31.87 |
| 2 | 优化前 | 57.23 | 132.22 | 156.67 |
| 2 | 优化后，中间版本 | 110.31 | 68.20 | 88.58 |
| 2 | 优化后，最终源码 | 106.99 | 72.10 | 85.50 |

最终源码的 2-job 带宽提高 86.9%，平均延迟降低 45.5%。1-job 出现约 9.6% 的负向波动，
两次重复结果一致，不能声称单线程无回退；最终源码在覆盖写中直接按
`newly_reserved == 0` 跳过 allocation-goal 分支，尚无证据表明该波动由本次代码
直接导致。

开启 `pipeline_profile` 的 2-job 诊断结果进一步确认第二次树路由已经消失：

| 阶段 | 优化前 (us/page) | 优化后 (us/page) | 变化 |
|---|---:|---:|---:|
| allocation goal | 65.334 | 0.058 | -99.91% |
| write plan 总计 | 61.448 | 64.035 | +2.587 |
| SSD extent tree | 59.713 | 61.832 | +2.119 |
| 前台总计 | 138.830 | 79.600 | -59.230 |

该诊断镜像对应审查前的中间版本：它已消除树查询，但仍执行约 0.058 us/page 的
hint load 和 extent 类型扫描。最终源码在 `newly_reserved == 0` 时直接跳过整个
allocation-goal 阶段，因此不会比表中的该阶段更多。

优化后一次 write plan 平均仍读取 2.958 个 extent-tree 节点，每次节点读取平均
10.442 us。这是必要的 baseline 查询，也是后续 inode-local range index/leaf
cache 应解决的剩余主要开销。

优化验证证据目录：

- release 1-job：`/home/zn/asterinas-build-env/rnvpce-allocation-goal-20261104/release-j1`；
- release 1-job 重复：`/home/zn/asterinas-build-env/rnvpce-allocation-goal-20261104/release2-j1`；
- release 2-job：`/home/zn/asterinas-build-env/rnvpce-allocation-goal-20261104/release-j2`；
- profiling 2-job：`/home/zn/asterinas-build-env/rnvpce-allocation-goal-20261104/profile-j2`；
- 最终源码 release 2-job：`/home/zn/asterinas-build-env/rnvpce-allocation-goal-20261104/final-release-j2`；
- profiling ISO SHA-256：`7f0b891c96b6fc21320358ea87be6778d87c654308d834ccded7c6408dd7d920`。
- 最终源码 release ISO SHA-256：`ae655b67c47978b8d78fc8311feacfc0d80806294994a15f613fe97203e9aaaa`。

五次优化后运行的 fio、sync、umount 和只读 e2fsck 均成功。封装器仍只因独立的
多 hctx 分布门槛返回 3，不影响本节的文件系统路径验证。
