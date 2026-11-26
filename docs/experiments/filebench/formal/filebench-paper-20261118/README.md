# Filebench EXT4 / rNVPCe-SU 对比实验

## 目的

在 Asterinas 上对比 EXT4 buffered writeback 与 rNVPCe-SU + rTIO 的
Filebench 宏观性能。该实验使用 NVMe 和 SATA 两类真实设备，结果不包含
rNVPC-WAL。

## 固定配置

- 文件系统：EXT4、rNVPCe-SU。
- 设备：NVMe `/dev/nvme1n1p2`，SATA `/dev/sda4`。
- 每个测试点重新格式化目标分区；EXT4 启用 journal，rNVPCe 使用 nojournal。
- 每个 workload 使用 16 个 Filebench thread。
- 文件数 10000，平均文件大小 128 KiB，数据集约 1.25 GiB。
- 每点运行 60 秒、独立重复 3 次，汇总图采用 3 次结果的中位数。
- 总数据缓存配额约为数据集的 10%，即 128 MiB：
  - EXT4：128 MiB DRAM PageCache。
  - rNVPCe：64 MiB DRAM PageCache + 64 MiB NVPage 数据池。
  - rNVPCe 的 64 MiB NVM 元数据保留区不计入数据缓存配额。
- VM 使用 64 vCPU 和 8 GiB 内存。
- NVMe 使用 4 个 rTIO worker、4 个 virtqueue 和 4 个 QEMU I/O thread。
- SATA 使用 1 个 rTIO worker、1 个 virtqueue 和 1 个 QEMU I/O thread。

## Workload

| 名称 | 每线程每轮操作 | 读写关系 |
| --- | --- | --- |
| `fileserver` | 整文件读取（1 MiB I/O），两次 16 KiB 随机追加，`stat` | 1:2 |
| `varmail` | 整文件读取，16 KiB 分块重写，`fsync`，`stat` | 邮件重写语义 |
| `webserver-10r1w` | 十次整文件读取（1 MiB I/O），一次 16 KiB 随机追加 | 10:1 |
| `webserver-1r1w` | 一次整文件读取（1 MiB I/O），一次 16 KiB 随机追加 | 1:1 |

Filebench 1.5-alpha3 在每个计时阶段前创建并预分配文件集。最终指标为
Filebench 报告的 `ops/s` 和 `MB/s`。每个测试点必须同时满足：Filebench
退出成功、guest `sync` 成功、卸载成功、宿主侧只读 `e2fsck -fn` 返回 0。

## 复现

警告：以下命令会反复清空并格式化 `/dev/nvme1n1p2` 和 `/dev/sda4`。

```bash
cd /mnt/data/zn/Rust_EXT4/os_com_rnvpc_rebuild
ROOT_ART=$PWD/docs/experiments/filebench/formal/filebench-paper-20261118 \
SOURCE_NVME=/home/zn/asterinas-build-env/filebench-paper-20261118/source-nvme \
SOURCE_SATA=/home/zn/asterinas-build-env/filebench-paper-20261118/source-sata \
bash docs/experiments/filebench/formal/filebench-paper-20261118/scripts/run-matrix.sh
```

已完成且存在 `result.json` 的测试点会被跳过；残缺的测试点不会自动覆盖，
以免把失败现场误当成有效数据。

## 输出

- `<device>-<mode>-<workload>-r<repeat>/`：每点的串口日志、配置、
  QMP/I/O thread 信息、`e2fsck` 结果和解析结果。
- `raw-results.csv`：绘图使用的所有独立重复原始数据。
- `summary.csv`、`summary.txt`：按设备、文件系统和 workload 汇总的中位数。
- `nvme-filebench-ops.png`：NVMe `ops/s` 对比图。
- `sata-filebench-ops.png`：SATA `ops/s` 对比图。
- `REPORT.md`：结果表、相对差异和测试过程中发现的问题。

测试实现位于
`docs/experiments/filebench/formal/filebench-paper-20261118/scripts/`。NVMe 与 SATA 镜像使用
同一代码，仅内核启动参数中的 `rtio_workers` 分别为 4 和 1。
