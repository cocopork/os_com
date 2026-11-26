# rNVPCe 128 KiB 顺序写延迟分解（2026-10-29）

本实验测量首次分配快速路径和连续 NVPage 分配修复后的 128 KiB 单线程顺序写。
结论是：前台逐页 extent tree 查询已经消除；当前主要剩余开销转移到后台
SSD baseline 结构发布，以及它与前台共享 publication guard 所产生的等待。

## 实验条件

- 分支：`recovery/rnvpce-page-redirection`，包含当前未提交修改。
- 设备：已授权的 `/dev/nvme1n1p1`，实验前格式化，实验后卸载并运行
  `e2fsck -fn`。
- guest：64 vCPU、8 GiB、单 virtio-blk 队列、队列深度 1024，沿用 CPU 隔离配置。
- rNVPCe：Native 模式，NVM pool 128 MiB，metadata reserve 64 MiB，data limit
  64 MiB，低水线 32 MiB，fast persist 模拟，nojournal。
- fio：buffered `psync`，`direct=0`，单线程，128 KiB，顺序写 2 GiB。
- 诊断镜像临时启用 `PIPELINE`、`LATENCY` 和 `NVPAGE-BATCH`；实验结束后三个
  开关均已恢复为关闭。

fio 完整写入 2 GiB，正常卸载且文件系统检查通过。诊断镜像达到 127.91 MiB/s，
含最终排空为 94.46 MiB/s；对应无探针 release 镜像为 135.82 MiB/s 和
98.79 MiB/s。

## 顶层阶段

表中总时间是各调用的累计 CPU/等待时间。前台、flush 和 completion 并发执行，
部分阶段也互相包含，因此不能把各行相加作为墙钟时间。直方图分位数是 2 的幂次
桶上界，不是精确样本值。

| 阶段 | 调用次数 | 累计时间 | 平均每次 | P50 上界 | P99 上界 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 前台 write 总计 | 16,384 | 15.968 s | 974.6 us | 1.049 ms | 4.194 ms |
| 前台持久化事务 | 16,384 | 3.589 s | 219.0 us | 262 us | 2.097 ms |
| 容量等待 | 57 | 0.270 s | 4.740 ms | 4.194 ms | 67.109 ms |
| flush round | 3,884 | 4.021 s | 1.035 ms | 524 us | 8.389 ms |
| 异步数据就绪等待 | 3,884 | 1.112 s | 286.4 us | 262 us | 2.097 ms |
| completion queue 等待 | 3,884 | 0.087 s | 22.3 us | 32.8 us | 65.5 us |
| completion 总计 | 3,884 | 0.627 s | 161.3 us | 262 us | 524 us |
| 设备 flush | 3,884 | 0.538 s | 138.5 us | 262 us | 262 us |
| SSD baseline extent 发布 | 509 | 15.447 s | 30.347 ms | 16.777 ms | 268.435 ms |

3,884 个 flush round 迁移 524,288 页，平均每轮 135.0 页。设备 flush 和
completion 本身均不足以解释前台延迟；509 次 extent 发布的累计时间最大。

## 前台 write 分解

| 阶段 | 调用次数 | 累计时间 | 平均延迟 |
| --- | ---: | ---: | ---: |
| write transaction body | 16,384 | 2.846 s | 173.7 us/request |
| NVPage 初始化与数据登记 | 16,384 | 1.681 s | 102.6 us/request |
| redirect 批量登记 | 16,384 | 0.929 s | 56.7 us/request |
| NVPage 清零 | 524,288 | 0.390 s | 0.744 us/page |
| 用户数据复制 | 524,288 | 0.136 s | 0.259 us/page |
| SSD 容量预留 | 16,384 | 0.076 s | 4.63 us/request |
| inode 发布 | 16,384 | 0.045 s | 2.77 us/request |
| extent 更新 | 16,384 | 0.017 s | 1.02 us/request |
| inode 读取 | 16,384 | 0.023 s | 1.41 us/request |
| extent lookup | 16,384 | 0.0037 s | 0.223 us/request |

`extent lookup` 从上一轮的 8.293 s 降到 3.66 ms，说明首次写容量计算和提交阶段的
两轮逐页查树已经消除。当前 transaction body 仅约 174 us/request，但前台总延迟
仍约 975 us/request。

publication guard 共获取 16,952 次，累计等待 9.879 s，平均 582.7 us；累计持有
20.032 s，平均 1.182 ms。该指标同时包含前台和后台调用，不能直接当作纯前台时间，
但其量级可以解释 transaction body 之外的大部分前台延迟。

## flush 与发布分解

| 阶段 | 调用次数 | 累计时间 | 平均延迟 | 平均处理页数 |
| --- | ---: | ---: | ---: | ---: |
| 候选选择 | 28,586 | 1.645 s | 57.5 us/attempt | 18.3 页/attempt |
| SSD target 准备 | 3,884 | 1.548 s | 398.6 us/round | 135.0 页/round |
| data BIO 提交 | 3,884 | 0.634 s | 163.3 us/round | 135.0 页/round |
| completion 比较 | 9,851 | 4.250 s | 431.4 us/range | 53.2 页/range |
| descriptor 清理 | 9,851 | 0.577 s | 58.6 us/range | 53.2 页/range |
| baseline 结构更新 | 509 | 9.963 s | 19.574 ms/session | 1,030 页/session |
| 完整 extent 发布 | 509 | 15.447 s | 30.347 ms/session | 1,030 页/session |

完整 extent 发布中，baseline 结构更新约占 64.5%，completion 比较约占 27.5%，
descriptor 清理约占 3.7%。三者合计 14.79 s，与 15.45 s 的完整发布时间接近。

## 与修复前诊断对比

| 指标 | 连续分配修复后、查树修复前 | 当前 | 变化 |
| --- | ---: | ---: | ---: |
| 前台 write 累计 | 32.538 s | 15.968 s | -50.9% |
| 前台持久化事务 | 12.725 s | 3.589 s | -71.8% |
| 前台 extent lookup | 8.293 s | 0.0037 s | -99.96% |
| 容量等待 | 0.615 s | 0.270 s | -56.1% |
| flush round | 4.516 s | 4.021 s | -11.0% |
| baseline 结构更新 | 6.562 s | 9.963 s | +51.8% |
| publication guard 等待 | 6.530 s | 9.879 s | +51.3% |
| 完整 extent 发布 | 13.991 s | 15.447 s | +10.4% |

前台加速后更快地产生 dirty NVPage，后台 baseline 发布与前台的重叠和竞争随之增加。
因此下一步不应继续优化已经降到微秒级的前台查树，而应拆分并减少
`structural_baseline` 的 509 次结构更新，或者把不要求立即写回 SSD 的权威 extent
发布从 NVPage 回收临界路径中移出。

## 证据

- 原始串口日志、manifest、状态、文件系统检查：
  `/home/zn/asterinas-build-env/initial-commit-fastpath-20261029/diagnostic/`
- 完整指标：`pipeline-metrics.csv`
- 顶层阶段：`pipeline-stages.csv`
- 无探针吞吐数据：
  `/home/zn/asterinas-build-env/initial-commit-fastpath-20261029/final/results.csv`

原始日志和磁盘镜像保留在 Git 仓库外；本报告是可追踪的实验摘要。

## flush 频率与并发窗口实验

以无探针 release 对照 `135.818 MiB/s`、排空 `98.794 MiB/s` 为基准，分别只改变
一个变量。所有样本均为 `/dev/nvme1n1p1`、单线程 128 KiB buffered 顺序写、2 GiB、
64 MiB 上限/32 MiB 低水位、virtio 队列深度 1024；有效样本均完成 sync、卸载及
只读 `e2fsck`。

| 实验 | fio 带宽 | 相对对照 | 排空带宽 | 相对对照 | 结论 |
| --- | ---: | ---: | ---: | ---: | --- |
| completion 就绪批次共用一次设备 flush | 136.752 MiB/s | +0.7% | 96.286 MiB/s | -2.5% | 3,165 批只有 10 次屏障可合并，无实际收益，已回退 |
| 迁移高水位从 64 MiB 提前到 48 MiB | 124.817 MiB/s | -8.1% | 92.838 MiB/s | -6.0% | 增加前后台 publication 重叠与争用，已回退 |
| 同批逻辑/物理连续 baseline 预合并 | 129.604 MiB/s | -4.6% | 93.388 MiB/s | -5.5% | 字段更新次数减少，但 publication 临界区未解除，已回退 |

flusher 在成功提交后本来就零延迟继续运行，10 ms 仅用于空闲或无进展退避；代码中
也不存在限制提交量的全局 `nv_in_flight` 窗口。completion 合并诊断中设备 flush
累计仅约 0.52 s，而 extent publication 仍约 13.87 s。因此继续提高唤醒频率或增加
全局 in-flight 不会填补设备带宽，反而可能扩大前后台争用。下一优化点应是缩短或
拆除 SSD baseline publication 的串行权威树更新临界区，而不是继续调度参数扫点。

实验原始产物保存在仓库外：

- `/home/zn/asterinas-build-env/coalesced-completion-flush-20261029/`
- `/home/zn/asterinas-build-env/early-migration-watermark-20261029/`
- `/home/zn/asterinas-build-env/baseline-batch-coalesce-20261029/final6/`
