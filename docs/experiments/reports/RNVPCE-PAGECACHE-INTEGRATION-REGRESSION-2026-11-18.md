# rNVPCe PageCache 集成回归实验（2026-11-18）

## 目的

验证 PageCache 热读索引接入后，原有写路径和冷读路径没有性能回退。聚焦矩阵为：

- append write：128 KiB，1/16 jobs；
- overwrite randwrite：4 KiB，1/16 jobs；
- cold randread：4/16/64/256 KiB，1 job；
- 每个测试点重复 3 轮，数据集 2 GiB；
- 64 vCPU、8 GiB、4 个 virtqueue/I/O thread、4 个 rTIO worker；
- rNVPC 池 128 MiB，其中元数据 64 MiB、数据 64 MiB；
- 设备 `/dev/nvme1n1p2`，每个测试点前重新格式化并在测试后执行 `e2fsck`。

测试矩阵由 `full-e2e-sweep-20261117/sweep.py pagecache-regression-list` 生成，避免手工测试参数漂移。

## 测试版本

- Git 基线：`7ebc1773d44688955db8739441b1f34017c8320e` 加本报告对应的未提交工作树改动；
- 最终测试 ISO SHA-256：`a8aea6d4e3539d86d42dd44328cc0f7ee9aa2d5a58d9566fe6bed1d79da440f9`；
- 当前结果：`/home/zn/asterinas-build-env/pagecache-regression-20261118/results-final/`；
- 历史对照：`/home/zn/asterinas-build-env/pagecache-regression-20261118/results-baseline/`；
- 构建输入：`/home/zn/asterinas-build-env/pagecache-regression-20261118/source-final/`。

## 结果

测试期间宿主机长期处于高负载状态，最终检查时 load average 为
`50.56/46.58/34.84`。第 1 轮的 16-job 测试受到明显干扰，例如 4 KiB
overwrite 只有 170.91 MiB/s，而相同镜像第 2、3 轮分别为 567.47 和
542.66 MiB/s。因此下表使用第 2、3 轮均值与历史 3 轮均值比较；第 1 轮仍保留在
原始 CSV 中，不作为代码性能结论。

| 负载 | 本次稳定轮次均值 MiB/s | 历史均值 MiB/s | 变化 |
|---|---:|---:|---:|
| append 128 KiB, 1 job | 2877.86 | 2550.76 | +12.8% |
| append 128 KiB, 16 jobs | 961.05 | 865.75 | +11.0% |
| overwrite 4 KiB, 1 job | 193.47 | 197.72 | -2.1% |
| overwrite 4 KiB, 16 jobs | 555.07 | 518.20 | +7.1% |
| cold randread 4 KiB | 34.94 | 36.46 | -4.2% |
| cold randread 16 KiB | 119.79 | 36.01 | +232.6% |
| cold randread 64 KiB | 294.99 | 36.67 | +704.5% |
| cold randread 256 KiB | 388.68 | 37.56 | +934.8% |

4 KiB overwrite 和 4 KiB cold read 均处于约 5% 的环境波动范围；其余测试点没有
回退。大块 cold read 的提升来自读路径能够按请求范围复用 PageCache/redirect 查询，
不再把一个大请求退化为同等数量的独立 extent 路由。

## 实现调整

本轮验证中发现并修复了三项会掩盖 PageCache 收益的问题：

1. redirect 持久化接口单批最多接受 128 条记录。completion 现在按 128 条分块发布，
   消除了约 64 MiB 后出现的 `EINVAL/EIO`。
2. 写入但从未进入 PageCache 的 NVPage 在 SSD 权威发布后直接按 entry 批量释放；只有
   PageCache 仍持有的页才保留为 CLEAN NV，避免纯写负载把整个 NV 池占满后转入前台回收。
3. 写路径不再为每个写页立即创建 XArray 槽。第一次读取通过 redirect 冷路径发布槽，
   后续读取走 PageCache 热路径；从未填充 PageCache 的 inode 通过单调标志跳过无意义的
   XArray 写锁和范围失效。

## 正确性验证

- `python3 -m unittest .../test_sweep.py`：8 项通过；
- `cargo fmt --all -- --check`：通过；
- `git diff --check`：通过；
- `cargo osdk test ext4_research_routes`：1 个聚合测试通过，内部 31 个 EXT4/rNVPCe
  场景全部通过，包括未缓存页立即释放和缓存页保留为 CLEAN NV；
- 完整 474 项内核测试曾执行，但仍包含已有的全局状态隔离和旧策略断言失败，不能宣称
  全套通过；本轮修改覆盖的聚焦回归均已单独通过。

## 结论

PageCache 集成没有造成目标写路径或单线程冷读路径的可复现回退。当前实现把 PageCache
作为延迟填充的热读索引：首次访问仍遵守原有 extent tree + NV redirect 冷路径，命中后
才走 PageCache；写入持久性和迁移权威关系仍由 redirect/extent 系统负责，两套索引没有
合并为同一事务。

## 多线程冷读消融（补充）

### 文件隔离关系

thread sweep 不是所有 job 共享一个文件。准备阶段创建 `f.0` 到 `f.(jobs-1)`，fio 使用
`--filename_format='f.$jobnum'`，因此每个 job 使用独立文件、inode 和
`RnvpcePageCache/XArray`。它排除了同一 inode 的 XArray 写锁竞争，但没有隔离物理页
分配器、堆分配器、块设备队列、调度器等系统资源。

### 对照与消融结果

NVMe 4 KiB cold randread，每点 2 GiB，表中为 3 轮均值：

| 实现 | 4 jobs | 16 jobs | 24 jobs | 32 jobs | 单位 |
|---|---:|---:|---:|---:|---|
| 当前 rNVPCe PageCache | 123.2 | 356.9 | 430.9 | 268.7 | MiB/s |
| 禁止 clean page 驻留 | 114.6 | 260.1 | 304.4 | 209.8 | MiB/s |
| 星绽原生 EXT4 PageCache | 118.1 | 272.9 | 270.4 | 236.2 | MiB/s |

证据目录：

- 当前完整矩阵：`docs/experiments/test_results/full-e2e-cold-read-pagecache-20261118`；
- 禁止驻留消融：`/home/zn/asterinas-build-env/rnvpce-pagecache-ablation-20261118/no-retain`；
- 原生 EXT4 PageCache 对照：`/home/zn/asterinas-build-env/rnvpce-pagecache-ablation-20261118/ext4-control`。

禁止驻留版本仍执行 CachePage 分配、XArray Loading 发布、BIO 和数据复制，只在读取后
立即删除槽。它在所有线程数上更慢，说明 clean page 长期驻留不是退化主因；反而是反复
删除、释放和重新分配增加了开销。原生 EXT4 PageCache 在 16 jobs 以后同样停止扩展，
表明星绽通用的 4 KiB cold-miss/PageCache/I/O 路径存在并发上限，不是 rNVPCe 单独引入
的一把 inode 全局锁。

### 低扰动阶段探针

诊断镜像按逻辑页号抽样约 1/256 的 cold miss。第一轮探针结果如下：

| jobs | route | CachePage alloc | BIO submit/wait | XArray publish | 窗口总计 |
|---:|---:|---:|---:|---:|---:|
| 4 | 41.59 | 0.101 | 109.77 | 0.965 | 152.74 |
| 16 | 17.74 | 0.107 | 135.06 | 1.124 | 154.39 |
| 32 | 98.56 | 0.155 | 230.38 | 4.906 | 334.51 |

单位为 `us/page`，每点 2048 个样本。32 jobs 相比 16 jobs 增加的主要部分是 BIO
提交/等待（约 95.3 us）和 route（约 80.8 us）；物理页分配仅增加约 0.05 us，不能
解释约 180 us 的总差值。

第二轮把 route 继续拆开。不同轮次的绝对值受宿主机负载影响明显，但三个线程点中
`current_page_mapping()` 均占 route 的 98% 以上；热索引预留约 0.2--0.5 us，NV
redirect 查询约 0.2--0.4 us。`current_page_mapping()` 每个冷页都会重新读取权威 inode
并遍历持久化 extent tree，这是 rNVPCe 自己仍可优化的重复工作。证据目录为
`/home/zn/asterinas-build-env/rnvpce-pagecache-ablation-20261118/probe-route`。

### 判断

1. **不是共享页分配器主导。** 星绽 frame allocator 有 per-CPU 小页 cache 和 per-CPU
   buddy pool，探针中的实际分配时间也稳定在亚微秒。
2. **存在 rNVPCe 特有的可优化开销，但不是 correctness bug。** 冷页仍通过
   `current_page_mapping()` 重读权威 inode 并遍历 extent tree；应把 SSD mapping/extent
   route 缓存在 inode 的冷读窗口或批量映射结果中，而不是逐页重查。
3. **同时存在星绽 PageCache/I/O 栈的共性扩展限制。** 原生 EXT4 PageCache 对照也在
   高线程下退化，且当前 rNVPCe 在 16/24 jobs 反而更快；因此不能把全部损失归因于
   rNVPCe 的 XArray 实现。32 jobs 的主要剩余延迟落在 BIO 提交、完成等待和调度。

正式 `full-e2e-20261117` 数据没有用本轮结果覆盖：新实现的 thread sweep 尚未达到
“符合预期”的覆盖条件。
