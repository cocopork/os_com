# rNVPCe Native 页描述符重定向实现与实验（2026-10-27）

本轮完成 Native 模式重构：NVM/SSD 权威位置由每页双副本 descriptor 表达，DRAM
按连续逻辑页合并重定向范围；前台覆盖、rTIO claim、completion 和恢复不再依赖
`NVPage -> extent leaf` reverse edge。EXT4 baseline extent tree 只保存 SSD 目标，v5
持久镜像拒绝旧版 NV extent target 和 v4 镜像。

## 正确性

- `cargo fmt --all -- --check`、`git diff --check`、普通及 ktest 配置检查通过。
- 完整 KVM ktest 在 1 vCPU 串行配置下为 `363 passed; 0 failed`。覆盖
  descriptor data/copy/selector 故障边界、重复恢复、
  旧镜像拒绝、无 reverse edge 恢复、FAST entry 更新、逻辑 split 和 FAIR 节点 split。
- 8 vCPU 并行运行曾使 4 项既有 PageCache/cache-pressure 测试因全局测试状态串扰
  失败；改为仓库既有的串行口径后全部通过。这不是 Native 路径失败，但并行隔离
  仍是测试基础设施的已知缺口。
- 9 项仅适用于 v4 混合 NV extent 驻留的历史测试退出活动集；SSD-only FAST/FAIR
  结构测试仍执行。
- 四个 2 GiB 实机点均 `fio error=0`、sync 和卸载成功，`e2fsck -fn` 返回 0。

首轮 128 KiB/1 线程测试发现 checkpoint 将 `SSD(0)` 占位 entry 当成物理映射，
与已经导出的 native extent 冲突并使卸载失败。新增跨多次 split 的回归测试后，
`visit_ssd_extents` 只导出非零、written SSD baseline；同配置复测及后续矩阵全部
通过。`SSD(0)` 仍仅表示尚无 SSD baseline，不能提交给原生 EXT4 extent tree。

## 实验配置

- 设备：已授权且未挂载的 `/dev/nvme1n1p1`，每点重新格式化 4 KiB EXT4。
- QEMU/KVM：64 vCPU、8 GiB，virtio-blk 单队列深度 1024；fio CPU 0-31，
  rTIO 使用 kernel CPU 32-59，virtio 提交 CPU 62。
- rNVPCe：pool 128 MiB、metadata reserve 64 MiB、data high watermark 64 MiB、
  low watermark 32 MiB、fast persist 模拟、运行时 nojournal。
- fio：buffered psync、direct=0、fsync=0、iodepth=1、总量 2 GiB；随机写先以
  direct I/O 预填充。诊断开关开启，因此下列带宽不是无探针 release 数字。

## 初始吞吐矩阵

“含 drain”从 fio 开始计时到最终 sync 返回，不含卸载。每个配置一轮。

| 工作负载 | 线程 | fio MiB/s | 含 drain MiB/s | 结果 |
| --- | ---: | ---: | ---: | --- |
| 4 KiB randwrite | 1 | 174.46 | 162.03 | clean |
| 4 KiB randwrite | 16 | 219.30 | 201.77 | clean |
| 128 KiB write | 1 | 25.90 | 24.54 | clean |
| 128 KiB write | 16 | 35.99 | 33.14 | clean |

## 最终审查修复后测点

完成 generation/owner 原子化、恢复 credit 相位、低水位 progress waiter、并发 reader
pin 延迟回收和 pending redirect reservation 清理后，重新构建 release 镜像并复测
128 KiB 顺序写。该测点使用与上表相同的设备和 2 GiB 配置。

| 工作负载 | 线程 | fio MiB/s | 含 drain MiB/s | 结果 |
| --- | ---: | ---: | ---: | --- |
| 128 KiB write | 1 | 251.84 | 158.76 | fio error=0；sync/卸载成功；e2fsck clean |

其中 fio 写入 2 GiB 用时 8.132 s；从 fio 开始到最终 sync 返回为 12.90 s。单轮结果
主要证明最终一致性修复没有重新引入早期的 25.90 MiB/s 退化，不作为统计显著性结论。

与 repair-image 报告的单轮 rNVPCe 相比，随机写 fio 分别提高约 43.5% 和 73.5%；
顺序写分别降低约 79.3% 和 62.5%。与本分支重构前的批量 FAST/FAIR 诊断点
45.6/198.5 MiB/s 相比，顺序写分别降低约 43.2% 和 81.9%。这些数据来自不同
源码状态且只有单轮，只用于定位，不表示统计显著性。

## Native 粒度

以下为 128 KiB 顺序写。1 线程的 16,384 次用户写恰好形成 16,384 个 descriptor
发布批次，每批 32 页；16 线程同样发布 524,288 页。`candidate_pages` 和
`completion_comparisons` 高于实际页数表示并发覆盖导致部分旧代候选被事后验证
淘汰，不表示重复回收；最终都只回收 524,288 页。

| 指标 | 1 线程 | 16 线程 |
| --- | ---: | ---: |
| redirect 发布页/批 | 524,288 / 16,384 | 524,288 / 16,384 |
| candidate 范围/页 | 16,387 / 524,384 | 78,191 / 663,904 |
| completion 比较 | 524,384 | 663,904 |
| descriptor clear 批 | 14,911 | 15,000 |
| 回收页/物理连续段 | 524,288 / 16,665 | 524,288 / 60,211 |
| SSD baseline 结构发布 | 14,485 | 14,847 |

随机覆盖已有 SSD baseline 时，两个配置的 `structural_baseline_mutations` 都为 0，
证明普通覆盖没有修改 extent tree；新文件顺序写必须在首次迁出时建立 SSD baseline。

## 阶段延迟

累计时间会因前台和后台并行而重叠，不能直接相加为墙钟时间。平均值为累计时间
除以调用次数。

| 阶段 | 1线程 调用/累计/平均 | 16线程 调用/累计/平均 |
| --- | ---: | ---: |
| 前台 write | 16,384 / 80.44 s / 4.91 ms | 16,384 / 918.29 s / 56.05 ms |
| 前台持久化 | 16,384 / 20.42 s / 1.25 ms | 16,384 / 21.63 s / 1.32 ms |
| 容量等待 | 12,146 / 2.26 s / 186 us | 184,238 / 383.81 s / 2.08 ms |
| redirect descriptor 发布 | 16,384 / 0.69 s / 42.4 us | 16,384 / 1.33 s / 81.1 us |
| range claim | 16,387 / 1.43 s / 87.0 us | 78,191 / 3.57 s / 45.6 us |
| flush round | 14,488 / 22.50 s / 1.55 ms | 16,740 / 28.19 s / 1.68 ms |
| metadata BIO 提交 | 14,488 / 18.53 s / 1.28 ms | 16,740 / 20.79 s / 1.24 ms |
| completion 总计 | 14,488 / 78.78 s / 5.44 ms | 16,740 / 59.77 s / 3.57 ms |
| completion descriptor 比较 | 14,488 / 2.15 s / 149 us | 19,209 / 1.11 s / 58.0 us |
| descriptor clear | 14,911 / 0.44 s / 29.4 us | 15,000 / 0.73 s / 48.4 us |
| SSD baseline 结构发布 | 14,485 / 73.35 s / 5.06 ms | 14,847 / 31.77 s / 2.14 ms |

结论：reverse leaf 读取、leaf target 原地发布及逐页 completion 校验已从 Native
主路径移除，descriptor compare/clear 不是当前主要耗时。顺序新文件的瓶颈转移到
首次 baseline 建立：completion 仍为约 1.45 万个小批次分别执行 FAST/FAIR tree
更新和 inode root 发布；16 线程还出现显著容量等待。下一步应合并同 inode 的已完成
目标，在一次 publication 中提交更大的 SSD baseline 批次，而不是回退到 repair-image。

## 证据

- 原始 fio、串口、manifest 和 e2fsck：
  `/home/zn/asterinas-build-env/page-descriptor-redirection-20261027/final/`
- 新阶段计时复测：
  `/home/zn/asterinas-build-env/page-descriptor-redirection-20261027/timed/`
- 最终审查修复后实机串口：
  `/home/zn/asterinas-build-env/page-descriptor-redirection-20261027/verified/rnvpce-write-j1-2048m-final4.log`
- 首轮失败与冲突诊断保留在相邻 `diagnostic/` 和 `conflict/`，不纳入有效结果。
