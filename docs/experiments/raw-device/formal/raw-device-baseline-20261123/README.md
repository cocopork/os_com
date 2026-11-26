# Linux / 星绽裸设备基线（2026-11-23）

本目录保存直接提交与 IRQ 分布修复后，Linux 和星绽在 NVMe、SATA 裸设备上的完整复测结果。

## 配置

- NVMe：`/dev/nvme1n1p2`，8 virtqueue、8 IOThread；
- SATA：`/dev/sda4`，1 virtqueue、1 IOThread；
- VM：64 vCPU、8 GiB，`cache=none,aio=threads`，virtqueue depth 1024；
- fio：`psync`、`direct=1`、每 job `iodepth=1`、每点约 2 GiB；
- bs sweep：1 job，4/8/16/32/64/128/256/512/1024 KiB；
- thread sweep：4 KiB，1/4/8/12/16/20/24/28/32 jobs；
- workload：顺序 write、randwrite、randread；
- 每坐标重复 3 轮，共 216 个坐标、648 轮。

星绽镜像来自提交 `92d3f7269`，启用 `virtio_blk_direct_submit=on`，关闭 rNVPC/rTIO 和性能插桩：

```text
bad85ebf3d3ca3eb35eede56fa32fd3a3697af4d4cc183670e3c0590533dde10  selected.iso
```

## 关键结果

下表列出 thread sweep 的 32 线程点：

| 设备 | 负载 | 星绽 MiB/s | Linux MiB/s | 星绽相对 Linux |
|---|---|---:|---:|---:|
| NVMe | sequential write | 1154.1 | 1231.0 | -6.3% |
| NVMe | random write | 1193.5 | 1199.8 | -0.5% |
| NVMe | random read | 1047.4 | 991.8 | +5.6% |
| SATA | sequential write | 252.1 | 247.8 | +1.8% |
| SATA | random write | 253.4 | 250.1 | +1.3% |
| SATA | random read | 229.8 | 219.9 | +4.5% |

当前星绽裸设备路径与 Linux 已处于同一性能区间。旧 `raw-device-baseline-20261121` 中星绽高并发性能显著落后的结果不再代表当前驱动。

## 数据

- `results.csv`：648 轮原始解析结果；
- `aggregate.csv`：216 个坐标的均值、标准差、最小值和最大值；
- `raw/*/result.json`：每个坐标的三轮结果；
- `plots/bandwidth-bs-summary.png`；
- `plots/bandwidth-thread-summary.png`。
