# rNVPCe 文件布局与线程数交叉实验（2026-11-05）

## 结论

实验确认，原 thread sweep 中 1 job 到 2 jobs 的性能突降由 SSD baseline 文件布局变化
引起，不是单线程与多线程采用了不同的 rNVPCe、rTIO、virtio 或设备 I/O 机制。

当两个 1 GiB 文件依次顺序创建、每个文件只有 9 个 data extent 时，1/2 jobs 的随机
覆盖写分别达到 169.62/350.27 MiB/s，2 jobs 扩展倍率为 2.06，平均延迟从
22.67 us 略降至 21.95 us。相反，并发创建使两个文件各产生约 7,000 个 data extent；
即使只用 1 job 访问其中一个碎片文件，带宽也降至 57.96 MiB/s，平均延迟升至
67.03 us。

因此，原扫描在从 1 job 切到 2 jobs 时同时改变了两个变量：并发数，以及文件从低碎片
布局变为高度碎片化布局。2 jobs 是第一个暴露昂贵 extent-tree 路由的点；4 jobs 以后
并没有切回另一条快速路径，而是通过更多独立 inode 和 worker 并发摊薄了相同的单次
查询成本。

## 实验设计

采用 2×2 矩阵，每个 job 始终覆盖一个 1 GiB 文件：

| 布局 | 构造方式 | 1 job | 2 jobs |
|---|---|:---:|:---:|
| 碎片 | 两个 fio job 并发顺序创建两个 1 GiB 文件 | 测 `f.0` | 同时测 `f.0/f.1` |
| 连续 | 两次独立 fio 调用依次创建两个 1 GiB 文件 | 测 `f.0` | 同时测 `f.0/f.1` |

准备阶段使用 native EXT4、128 KiB direct 顺序写。之后挂载 rNVPCe，先执行一轮 4 KiB
随机覆盖写并 sync，再测量第二轮随机覆盖写。每个点重新格式化设备；测试完成后通过
宿主 `debugfs` 只读检查实际 extent 数量，而不是仅根据写入方式假设布局。

## 测试环境

- 设备：`/dev/nvme1n1p2`；
- release ISO SHA-256：
  `f04ff9c241fe7187d81bedd67bef4f54a120ea9bd912dcaf2c36326505c2a432`；
- rNVPCe 默认 4 个 rTIO worker，内核命令行未显式传入 `rtio_workers`；
- 4 个 virtqueue、4 个 QEMU IOThread，并绑定宿主 NVMe hctx 0--3；
- rNVPCe pool 128 MiB，metadata reserve 64 MiB，data limit 64 MiB；
- fio：4 KiB `randwrite`、buffered `psync`、iodepth 1/job；
- profiling 与 latency instrumentation 关闭；
- 每点一次，属于因果验证实验，不提供统计置信区间。

## 结果

| 文件布局 | jobs | 每文件 data extent | extent 索引块 | fio MiB/s | 含 sync MiB/s | 平均延迟 us | p99 us |
|---|---:|---:|---:|---:|---:|---:|---:|
| 碎片 | 1 | 6,956 / 6,957 | 22 / 22 | 57.96 | 56.39 | 67.03 | 83.46 |
| 碎片 | 2 | 7,370 / 7,370 | 23 / 23 | 102.86 | 100.59 | 74.85 | 114.18 |
| 连续 | 1 | 9 / 9 | 1 / 1 | 169.62 | 157.30 | 22.67 | 40.70 |
| 连续 | 2 | 9 / 9 | 1 / 1 | 350.27 | 326.63 | 21.95 | 44.80 |

### 固定线程数，只改变布局

| jobs | 碎片 MiB/s | 连续 MiB/s | 带宽提升 | 碎片延迟 us | 连续延迟 us | 延迟降低 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 57.96 | 169.62 | +192.6% | 67.03 | 22.67 | 66.2% |
| 2 | 102.86 | 350.27 | +240.5% | 74.85 | 21.95 | 70.7% |

### 固定布局，只改变线程数

| 布局 | 1-job MiB/s | 2-job MiB/s | 扩展倍率 | 1-job 延迟 us | 2-job 延迟 us |
|---|---:|---:|---:|---:|---:|
| 碎片 | 57.96 | 102.86 | 1.77× | 67.03 | 74.85 |
| 连续 | 169.62 | 350.27 | 2.06× | 22.67 | 21.95 |

碎片布局下也不存在 2-job 特有的串行化：吞吐仍随第二个线程提高 77%。连续布局则表现
出近乎理想的双线程扩展，并且没有平均延迟阶跃。

## 原因解释

rNVPCe 覆盖写的 `plan_range_write()` 每次都需要查询当前 inode 的 SSD baseline extent
tree。高度碎片化文件具有更深的树和更多叶节点；一次 4 KiB 随机覆盖写需要读取、复制、
校验并扫描更多持久化 `ExtentNodeRecord`。此前 allocation-goal 优化已经消除了覆盖写的
第二次树路由，但第一次必要路由仍然存在，因此双线程旧数据从约 132 us 降到约 72 us
后仍保留了布局相关开销。本轮连续 2-job 的 21.95 us 结果证明该剩余延迟不是由
rTIO 双 worker、virtqueue 或底层 NVMe 并发引入。

这也意味着原 thread sweep 不是纯粹的“固定数据布局下线程扩展性”测试，而是
“并发文件创建布局 + 线程扩展性”的组合测试。其数据仍能代表并发创建文件后的真实
工作负载，但不能单独用于判断 rNVPCe 多线程扩展性。后续正式 thread sweep 应复用
预先构造且 extent 数量一致的文件系统镜像，避免每个线程点重新生成不同布局。

## 正确性与机制门禁

四个点均满足：

- fio、sync、umount 返回 0；
- 每个 job 精确写入 1 GiB；
- 只读 `e2fsck -fn` 返回 0；
- 宿主多 hctx 分布门禁通过，最大单 hctx 占比为 25.8%--50.0%；
- 两个文件大小均为 1 GiB，extent 统计来自测试后的真实设备。

## 证据位置

- 原始数据：`/home/zn/asterinas-build-env/rnvpce-layout-cross-20261105/results`；
- 输入镜像：`/home/zn/asterinas-build-env/rnvpce-layout-cross-20261105/source`；
- 可复现实验脚本：
  `docs/experiments/scripts/rnvpce-layout-cross-20261105/`；
- 原 1/2-job 延迟分解：
  [RNVPCE-J1-J2-WRITE-PATH-LATENCY-2026-11-04.md](RNVPCE-J1-J2-WRITE-PATH-LATENCY-2026-11-04.md)。
