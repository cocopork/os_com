# 冷读优化后的写性能补测（2026-11-26）

本实验检查 PageCache/冷读路径改动是否影响写路径，比较 Linux EXT4、星绽 EXT4 和
Achilles（rNVPCe-SU + rTIO）。正式结论使用与旧写基线一致的 64 vCPU 配置；每点独立
格式化 NVMe 分区并重复三轮，完成后执行 `e2fsck -fn`。

## 配置

- 设备：`/dev/nvme1n1p2`，256 GiB；
- VM：64 vCPU、8 GiB，8 virtqueue、8 IOThread；
- fio：`psync`、buffered I/O、`iodepth=1`，每点总计 2 GiB；
- overwrite：4 KiB 随机覆盖写，1/16/32 jobs；计量前用 128 KiB direct 顺序写建立
  低碎片文件，因此计量期不分配文件数据块；
- append：128 KiB 顺序首次写，1/16 jobs，包含数据块和 extent 元数据分配；
- 星绽 PageCache：128 MiB；Linux fio cgroup：`memory.high=136 MiB`、
  `memory.max=512 MiB`、禁用 swap；
- 星绽 EXT4、Linux EXT4 使用 `fsync=0`；Achilles 使用 `fsync=1`，即每个 fio block
  后执行一次 `fsync()`。因此 EXT4 是异步 buffered-write 上限对照，不是等持久性比较。

星绽镜像 SHA-256：

```text
6b6afadde43ee4206ca5a9ecba657975b6f83ceb05e9c04300f07a0bcbaa8ef3  selected.iso
```

代码基于提交 `92d3f7269754f2a3b1e8ff5beb77c26688f94152` 上尚未提交的
PageCache/冷读路径改动。Linux 内核镜像 SHA-256 为
`06ea2dcca70d690e5ec99f10d5e68dff57827144e7f33cb354cb3f6a2eef16ba`。

## 结果

下表为 fio 计量期带宽三轮均值。括号内是相对
`fio/formal/full-e2e-20261123/results.csv` 中同配置旧星绽基线的变化；该旧矩阵没有
128 KiB、16 jobs append 点。

| 负载 | jobs | Linux EXT4 | 星绽 EXT4 | Achilles |
|---|---:|---:|---:|---:|
| 4 KiB overwrite | 1 | 313.4 MiB/s | 273.8 MiB/s（+14.0%） | 317.4 MiB/s（-0.2%） |
| 4 KiB overwrite | 16 | 332.7 MiB/s | 407.3 MiB/s（+4.9%） | 892.2 MiB/s（+12.8%） |
| 4 KiB overwrite | 32 | 334.5 MiB/s | 384.5 MiB/s（+23.2%） | 824.5 MiB/s（+1.9%） |
| 128 KiB append | 1 | 841.4 MiB/s | 1417.9 MiB/s（-3.9%） | 2034.9 MiB/s（-23.2%） |
| 128 KiB append | 16 | 1008.0 MiB/s | 2129.0 MiB/s | 962.6 MiB/s |

计入 fio 结束后全局 `sync` 的均值见 `summary.csv`。所有 45 次运行均完成预期 2 GiB
I/O，guest 正常卸载，宿主 `e2fsck -fn` 返回 0。

## 结论

1. **overwrite 没有整体回退。** Achilles 单线程与旧值基本一致，16 jobs 提升 12.8%，
   32 jobs 提升 1.9%；当前 16/32 jobs 带宽也分别是 Linux EXT4 的 2.68/2.46 倍。
2. **星绽 EXT4 没有写回退信号。** 三个 overwrite 点均不低于旧基线；128 KiB
   单线程 append 的 -3.9% 落在三轮波动范围内。
3. **Achilles 128 KiB append 存在明确回退。** 单线程从旧版 2648.0 MiB/s 降至
   2034.9 MiB/s，下降 23.2%。16 jobs 仅 962.6 MiB/s，且平均延迟达到 2.02 ms；
   旧矩阵没有同坐标，不能计算版本变化，但当前多线程扩展性明显弱于星绽 EXT4。
4. 冷读诊断使用的 40 vCPU 配置会把 Achilles overwrite 16/32 jobs 降到
   591/556 MiB/s，并把 append 1/16 jobs 降到 1536/840 MiB/s。该组只说明 Achilles
   对可用 vCPU 数敏感，未纳入正式回退判断。

## 文件

- `results.csv`：45 次运行的逐轮原始指标；
- `summary.csv`：三轮均值、标准差、旧基线及变化比例；
- `raw/`：每点的 fio JSON、QEMU 串口日志和 `e2fsck` 结果；
- `scripts/`：星绽矩阵扩展、Linux guest/runner 和聚合脚本。
