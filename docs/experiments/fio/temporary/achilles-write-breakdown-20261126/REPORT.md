# Achilles 4 KiB 写路径分解结果（2026-11-26）

## 端到端结果

| 负载 | jobs | 诊断带宽 MiB/s | 历史正式带宽 MiB/s | 星绽裸设备 MiB/s |
|---|---:|---:|---:|---:|
| append | 4 | 223.17 | 226.91 | 382.14 |
| append | 16 | 412.89 | 426.70 | 953.57 |
| append | 32 | 280.75 | 272.56 | 1154.10 |
| overwrite | 4 | 673.35 | 770.81 | 348.27 |
| overwrite | 16 | 778.52 | 782.82 | 987.21 |
| overwrite | 32 | 787.75 | 782.33 | 1193.54 |

append 的诊断值与历史三轮均值接近。overwrite 16/32 jobs 也接近；4 jobs 诊断值低约 13%，因此只用它判断阶段结构，不把单次绝对值视为新基线。裸设备是 direct psync，而 Achilles 先持久化到 NVM，4 jobs 下可高于裸设备；高并发时裸设备值主要用于说明驱动上限仍未用满。

## 累计服务需求占比

| 负载 | jobs | 元数据分配 | rTIO 软件 | 驱动 I/O | 容量等待 | 其余前台事务 |
|---|---:|---:|---:|---:|---:|---:|
| append | 4 | 18.9% | 5.1% | 20.9% | 0.0% | 55.1% |
| append | 16 | 25.0% | 3.1% | 10.1% | 0.0% | 61.9% |
| append | 32 | **62.8%** | 0.9% | 3.8% | 0.0% | 32.4% |
| overwrite | 4 | 0.0% | 8.4% | 43.0% | 0.0% | 48.5% |
| overwrite | 16 | 0.0% | 3.8% | 19.6% | 15.2% | 61.5% |
| overwrite | 32 | 0.0% | 2.0% | 10.9% | **20.3%** | 66.8% |

## 阶段时间

单位均为微秒/4 KiB 页。嵌套项用于定位，不能再次相加。

### Append

| 阶段 | 4 jobs | 16 jobs | 32 jobs |
|---|---:|---:|---:|
| 前台总时间 | 49.51 | 127.29 | 421.67 |
| SSD allocate（含块/位图） | 7.19 | 13.73 | 9.48 |
| allocation record 发布 | 3.90 | 14.79 | **163.17** |
| allocation record 回收 | 1.44 | 7.54 | **105.23** |
| write plan | 10.49 | 23.19 | 24.72 |
| persist（包含 allocation record） | 25.66 | 70.75 | **368.61** |
| rTIO 软件 | 3.45 | 4.52 | 3.92 |
| 驱动 I/O + flush | 14.01 | 14.77 | 16.95 |

32 jobs 的直接异常是 allocation record：发布和回收合计 268.40 us/page，占真实元数据分配服务时间的 96.5%。块分配和位图更新本身没有随线程数爆炸。代码为每个 4 KiB append 建立并立即回收持久化 `AllocationRecord`；该路径反复经过全局 metadata index/pool state 和 metadata frame 分配/清理。32 个前台线程在这里形成竞争，令 `persist` 从 70.75 增至 368.61 us/page。这解释了 append 从 16 jobs 的约 413 MiB/s 下降到 32 jobs 的约 281 MiB/s。

### Overwrite

| 阶段 | 4 jobs | 16 jobs | 32 jobs |
|---|---:|---:|---:|
| 前台总时间 | 18.49 | 67.11 | 138.48 |
| 实际 SSD 元数据分配 | **0.00** | **0.00** | **0.00** |
| SSD allocate 空调用 | 0.22 | 1.35 | 3.31 |
| write plan | 2.68 | 10.63 | 24.06 |
| span prepare | 3.26 | 12.50 | 25.98 |
| persist | 7.57 | 10.28 | 12.10 |
| publication/authority 前置协调 | 1.75 | 5.92 | 12.00 |
| 容量等待 | 0.01 | 13.31 | **32.31** |
| rTIO 软件 | 3.20 | 3.31 | 3.21 |
| 驱动 I/O + flush | 16.40 | 17.11 | 17.26 |

overwrite 的 `allocated_pages` 在三组均为 0，证明计量阶段没有 SSD 数据块或分配位图更新。rTIO 软件和驱动 I/O 每页时间也基本不随 jobs 增长。32 jobs 的额外前台延迟主要在：

1. 64 MiB NVM 池产生 32.31 us/page 容量等待；
2. plan/span 从约 3 us/page 增至 24--26 us/page；
3. publication/authority 协调从 1.75 增至 12.00 us/page。

因此 overwrite 的约 800 MiB/s 平台不是 completion 或设备服务变慢造成的，而是前台并发状态协调和容量反压限制了进入 rTIO 的速度。

## rTIO 与驱动观察

- append 的平均迁移批次均约 1024 页；overwrite 分别约 1024、897、969 页。32 jobs 没有退化成逐页小批次。
- 32 jobs overwrite 提交 8,377,457 个 BIO 迁移 8,388,608 页，仍接近一页一个 BIO。
- 四个 rTIO worker 几乎只使用 virtqueue 0--3；队列 4--7 基本空闲。活跃队列的最大 in-flight request 均达到 341，并发生 descriptor wait。
- `data_submit` 为 12--14 us/page，而从 batch 开始提交到全部 BIO 完成约 13--16 us/page。驱动面对的主要成本是逐页提交和队列反压，不是候选选择或 completion CPU 工作。

## 结论

当前两个负载的首要问题不同：

- **Append 32 jobs：持久化 allocation record 的逐页发布/立即回收及其全局元数据协调，是确定的第一瓶颈。** 应改为按 allocation range/batch 记录，或至少让一次事务共用一个 record，避免每 4 KiB 分配一个 4 KiB metadata frame 后马上清理。
- **Overwrite：没有元数据分配瓶颈。** 优先优化 plan/span 的并发索引访问、publication/authority 细粒度协调，以及容量反压；rTIO completion 本身不是首要瓶颈。
- 驱动仍有约 1.5 倍吞吐空间，但需在上述软件瓶颈之后处理：扩大 rTIO 对 8 个 virtqueue 的利用并合并连续 BIO，才可能逼近裸设备约 1.2 GiB/s。
