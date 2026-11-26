# Achilles 4 KiB 写路径分解实验（2026-11-26）

## 目的

对 NVMe 上 Achilles/rNVPCe-SU 的 4 KiB append 和 random overwrite 写路径做 4、16、32 jobs 横向分解，区分：

- SSD 元数据分配；
- rTIO 候选选择、迁移完成及权威状态转换；
- virtio/块设备提交、完成与设备 flush；
- 前台容量等待和其余前台事务。

## 配置

- 每 job 独占 1 GiB 文件，`psync`、`iodepth=1`、块大小 4 KiB。
- append 从空文件开始；overwrite 先建立低碎片文件，计量阶段不分配 SSD 数据块。
- 64 vCPU、8 virtqueue、4 个 rTIO worker。
- NVM data limit 64 MiB，测试期间每次 4 KiB 写执行 `fsync`。
- 每个点运行一次诊断构建；正式历史结果的三个重复样本仅用于核对插桩扰动。

原始串口日志位于：

```text
/home/zn/asterinas-build-env/achilles-write-breakdown-20261126/results/
```

分析脚本：`scripts/analyze.py`。运行后生成：

- `results/breakdown.csv`：服务需求分类及占比；
- `results/stage-details.csv`：各阶段微秒/页；
- `results/metrics.csv`：完整 pipeline 快照；
- `results/rtio.csv`：rTIO 批次计数；
- `results/virtio-queues.csv`：各 virtqueue 请求和等待计数；
- `results/baselines.csv`：裸设备和历史 Achilles 带宽。

## 口径

占比使用“每写入/迁移一页的累计服务需求”，而不是 fio 墙钟时间。前台与后台并行，不能把两者的计时直接称为端到端关键路径占比；该口径用于比较各模块消耗的总资源，并保持 4/16/32 jobs 可比。

- `metadata allocation`：真实发生分配时，包含 SSD allocate、allocation goal、持久化 allocation record 的发布和回收。overwrite 的分配单位数为 0，因此该项记为 0；空调用开销保留在其余前台事务中。
- `rTIO software`：candidate select + target prepare + completion。completion 包含本负载的权威状态转换和立即释放；显式 deferred reclaim 指标为 0。
- `driver I/O`：data batch 从开始提交到所有 BIO 完成的时间，加设备 durability flush；其中包含 virtqueue 提交和队列反压。
- `capacity wait`：前台等待 NVM 数据槽位。
- `foreground other`：前台总时间扣除真实元数据分配和容量等待后的其余事务。

`completion_queue_wait` 与 device flush/调度等待重叠，只作为诊断项列在 `stage-details.csv`，不重复计入占比。

结论见 [REPORT.md](REPORT.md)。
