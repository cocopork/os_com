# FAST-FAIR 批量 completion 实现与延迟分解（2026-10-27）

本轮保留 FAST-FAIR 的 8 字节原地 target 发布，将 rTIO 恢复到批量粒度：同一 leaf 一次 claim/读取多个 extent，同一 inode-completion 批量处理 credit、一次更新 reservation ledger，并批量回收 NVPage。单线程和 16 线程 128 KiB 顺序写均完整写入 2 GiB、正常卸载，最终 `e2fsck` clean。

## 实验配置

- 设备：`/dev/nvme1n1p1`，每个测试点重新格式化。
- 工作负载：128 KiB 顺序 buffered write，psync，direct=0，fsync=0，iodepth=1，总量 2 GiB。
- 并发：1 线程、16 线程；每线程独立文件。
- 虚拟机：64 vCPU、8 GiB，virtio queue depth 1024，启用历史 CPU 隔离配置。
- rNVPCe：pool 128 MiB，metadata reserve 64 MiB，data high watermark 64 MiB，low watermark 32 MiB，fast persist 模拟。
- 磁盘具有 journal feature，但 rNVPCe 运行时不启用 JBD2。
- 本报告延迟来自开启 `pipeline_profile` 的诊断镜像；测试后源码开关已恢复为关闭。

## 吞吐结果

“含 drain”按 `2 GiB / (sync_end - fio_start)` 计算，包含 fio 后等待 rTIO 排空和 sync，不包含 umount。

| 线程 | 修复前 fio | 修复后 fio | 变化 | 修复前含 drain | 修复后含 drain | 变化 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 36.6 MiB/s | 45.6 MiB/s | +24.7% | 32.3 MiB/s | 41.7 MiB/s | +29.1% |
| 16 | 160.2 MiB/s | 198.5 MiB/s | +23.9% | 98.0 MiB/s | 115.3 MiB/s | +17.6% |

## 整体流水线延迟

`累计时间` 是所有调用的累计 CPU/等待时间；并行阶段可能重叠，不能相加作为墙钟总时间。`平均` 为 `累计时间 / 调用次数`。前台计数对应 128 KiB fio 写调用，flush/completion 计数对应后台批次。

| 阶段 | 1线程调用 | 1线程累计 | 1线程平均 | 16线程调用 | 16线程累计 | 16线程平均 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 前台 write 总计 | 16,384 | 44.845 s | 2.737 ms | 16,384 | 160.572 s | 9.800 ms |
| 前台持久化部分 | 16,384 | 15.116 s | 0.923 ms | 16,384 | 32.279 s | 1.970 ms |
| 容量等待 | 242 | 1.935 s | 7.997 ms | 4,135 | 49.831 s | 12.051 ms |
| flush round 总计 | 512 | 6.409 s | 12.518 ms | 518 | 7.338 s | 14.166 ms |
| 候选选择/claim | 514 | 4.183 s | 8.137 ms | 520 | 4.362 s | 8.388 ms |
| SSD 目标准备 | 514 | 0.480 s | 0.934 ms | 520 | 0.390 s | 0.750 ms |
| data BIO 提交 | 512 | 0.547 s | 1.068 ms | 518 | 0.688 s | 1.327 ms |
| metadata BIO 提交 | 512 | 1.194 s | 2.331 ms | 518 | 1.895 s | 3.658 ms |
| 异步请求完成等待 | 512 | 1.207 s | 2.358 ms | 518 | 1.924 s | 3.715 ms |
| completion queue 等待 | 512 | 0.046 s | 0.089 ms | 518 | 0.138 s | 0.267 ms |
| completion 总计 | 512 | 1.503 s | 2.936 ms | 518 | 3.591 s | 6.933 ms |
| SSD flush | 512 | 0.074 s | 0.145 ms | 518 | 0.160 s | 0.309 ms |

## Completion 内部分解

`extent publish` 是一个 inode 在一次 completion batch 中的完整发布范围；其中的 leaf target 更新仍按 extent 执行 8 字节原地发布。因此单线程 `extent publish` 为 512 次，而 target persist 仍为 16,384 次。

| 阶段 | 1线程调用 | 1线程累计 | 1线程平均 | 16线程调用 | 16线程累计 | 16线程平均 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| extent publish 总计 | 512 | 1.345 s | 2.627 ms | 7,523 | 3.385 s | 0.450 ms |
| extent 分组 | 512 | 0.079 s | 0.153 ms | 7,523 | 0.053 s | 7.03 us |
| update 构造 | 16,384 | 0.025 s | 1.50 us | 16,399 | 0.027 s | 1.63 us |
| extent member 验证 | 16,384 | 0.113 s | 6.92 us | 16,399 | 0.195 s | 11.88 us |
| reverse lookup | 16,384 | 0.004 s | 0.24 us | 16,399 | 0.006 s | 0.34 us |
| leaf lock 等待 | 16,384 | 0.003 s | 0.18 us | 16,399 | 0.041 s | 2.50 us |
| leaf 读取 | 16,384 | 0.159 s | 9.73 us | 16,399 | 0.343 s | 20.92 us |
| leaf 验证 | 16,384 | 0.168 s | 10.23 us | 16,382 | 0.338 s | 20.62 us |
| 8B target persist | 16,384 | 0.011 s | 0.64 us | 16,382 | 0.316 s | 19.31 us |
| credit 扫描 | 512 | 0.121 s | 0.236 ms | 7,523 | 0.202 s | 26.90 us |
| credit descriptor 持久化 | 512 | 0.285 s | 0.556 ms | 7,523 | 0.345 s | 45.93 us |
| outcome 构造 | 512 | 0.107 s | 0.209 ms | 7,523 | 0.049 s | 6.51 us |
| reservation ledger 更新 | 512 | 0.003 s | 6.25 us | 7,523 | 0.274 s | 36.43 us |
| result/lease 汇总 | 512 | 0.041 s | 80.97 us | 7,523 | 0.033 s | 4.37 us |
| NVPage 批量回收 | 512 | 0.180 s | 0.352 ms | 7,523 | 1.058 s | 0.141 ms |

## 与修复前 FAST-FAIR 对比

下面仅比较本次直接优化的阶段。调用数是 profiler 外层作用域次数；credit descriptor 和回收在内部仍受持久层每批最多 128 页的限制。

| 指标 | 1线程修复前 | 1线程修复后 | 16线程修复前 | 16线程修复后 |
| --- | ---: | ---: | ---: | ---: |
| candidate select 累计 | 5.181 s | 4.183 s | 4.983 s | 4.362 s |
| credit persist 调用 | 16,384 | 512 | 16,383 | 7,523 |
| credit persist 累计 | 0.153 s | 0.285 s | 0.241 s | 0.345 s |
| ledger 调用 | 16,384 | 512 | 16,383 | 7,523 |
| ledger 累计 | 0.055 s | 0.003 s | 0.700 s | 0.274 s |
| reclaim 外层调用 | 16,384 | 512 | 16,383 | 7,523 |
| reclaim 累计 | 0.116 s | 0.180 s | 1.189 s | 1.058 s |

## 结论

- 批量 ledger 已生效。单线程调用数下降 96.9%，累计时间下降 94.2%；16 线程调用数下降 54.1%，累计时间下降 60.9%。
- 同 leaf 多 extent claim 使 candidate select 累计时间分别下降约 19.3% 和 12.5%。对应 ktest 验证两个 extent 共用 leaf 时只读取一次 leaf。
- credit persist 的外层调用数大幅下降，但累计时间没有下降。原因是每个 NVPage descriptor 仍需更新，只是改为最多 128 项一次 selector 事务；该批处理减少调用和 fence 粒度，不消除 descriptor 字节写入。
- 单线程 completion 仅累计 1.503 秒，明显低于前台 write 的 44.845 秒；16 线程前台累计 160.572 秒，容量等待 49.831 秒。当前端到端主要瓶颈仍在前台 FAST-FAIR 写路径及等待可用 NVPage，而不是 ledger。
- 16 线程的 16 个 inode 将同一 completion batch 拆成多个 inode 子批次，因此 ledger/reclaim 只能从约 16,384 次降到 7,523 次，批量程度低于单 inode 测试。

## 批处理边界故障及修复

首次实机运行在 64 MiB 高水线触发迁移后失败：completion 将约 1,024 个 credit descriptor 一次提交给只允许最多 128 项的双副本 selector，返回 `EINVAL`，继而导致 fio EIO 和无法卸载。修复后 credit descriptor 按 128 项持久化；所有 credit 子批次完成后才做一次 inode ledger 更新。最终两组测试均 `fio_status=0`、`umount_status=0`、`e2fsck_exit=0`。

## 证据

- [单线程原始日志](/home/zn/asterinas-build-env/fast-fair-batched-reclaim-20261027/diagnostic-fixed/rnvpce-write-j1-2048m.log)
- [16线程原始日志](/home/zn/asterinas-build-env/fast-fair-batched-reclaim-20261027/diagnostic-fixed/rnvpce-write-j16-2048m.log)
- [单线程运行清单](/home/zn/asterinas-build-env/fast-fair-batched-reclaim-20261027/diagnostic-fixed/rnvpce-write-j1-2048m-manifest.json)
- [16线程运行清单](/home/zn/asterinas-build-env/fast-fair-batched-reclaim-20261027/diagnostic-fixed/rnvpce-write-j16-2048m-manifest.json)
- [完整实验目录](/home/zn/asterinas-build-env/fast-fair-batched-reclaim-20261027/diagnostic-fixed)

验证：`cargo osdk check --ktests --package aster-kernel` 通过；批量 completion/ledger 和同 leaf 多 extent claim 定向 ktest 通过；两组实机测试最终文件系统检查 clean。诊断阶段累计时间用于定位，不能替代 release 模式多轮统计。
