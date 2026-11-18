# EXT4 / rNVPCe 双设备端到端扫描

## 结论

本轮在 NVMe 和 SATA 上完成 EXT4 与 rNVPCe-SU 的 overwrite randwrite、
append sequential write、cold randread 两类 sweep。正式矩阵共 648 次运行，
648 次均完成预期 2 GiB I/O、正常卸载并通过后置 `e2fsck -fn`，最终聚合为
216 个三轮均值点。rNVPC-WAL 已按要求从正式比较对象中移除。

主要结论如下：

1. rNVPCe 的覆盖写路径已经具备较好的多线程扩展性。NVMe 4 KiB overwrite
   在 4/16/32 线程分别为 603/556/551 MiB/s，对应 EXT4 的 324/358/293
   MiB/s；旧版本 32 线程的断崖下降未再出现。
2. rNVPCe 的大块写可以充分利用设备带宽。NVMe 1 MiB overwrite 为
   3087 MiB/s，SATA 128 KiB overwrite 为 487 MiB/s，分别是 EXT4 的
   3.34 倍和 1.43 倍。
3. 4 KiB append 首次分配仍是主要写入短板。NVMe 单线程 rNVPCe 仅
   198 MiB/s，EXT4 为 728 MiB/s；SATA 分别为 172 和 332 MiB/s。随着请求
   增大，固定元数据开销被摊薄，rNVPCe 在 128 KiB 及以上反而明显领先。
4. rNVPCe 冷读存在明确的大请求逐页串行问题。NVMe 的单线程 cold randread
   从 8 KiB 到 1 MiB 始终只有约 36--42 MiB/s，而平均延迟随页数近线性增长；
   1 MiB 时为 23.1 ms。SATA 上也复现了同一现象。4 KiB thread sweep 却能
   正常扩展，说明问题位于单个大请求的 rNVPCe 读路径，而不是块层并发能力。
5. NVMe EXT4 的 buffered append 和大块写存在较大轮间波动。报告与图片保留
   三轮原始点，不用单轮峰值下结论。

## 测试配置

| 项目 | 配置 |
|---|---|
| 设备 | NVMe `/dev/nvme1n1p2`；SATA `/dev/sda4` |
| 文件系统模式 | EXT4 journal；rNVPCe-SU nojournal + rTIO |
| VM | 64 vCPU、8 GiB DRAM |
| 块设备 | 4 virtqueue、4 独立 IOThread，分别固定到 CPU 60--63 |
| rTIO | 4 worker，CPU isolation 开启 |
| NVM | 128 MiB pool、64 MiB metadata reserve、64 MiB data limit |
| fio | `psync`、buffered I/O、iodepth=1、每点总数据约 2 GiB |
| thread sweep | 4 KiB；1、4、8、12、16、20、24、28、32 jobs |
| bs sweep | 1 job；4、8、16、32、64、128、256、512、1024 KiB |
| 重复次数 | 每点 3 轮 |

overwrite 与 cold randread 在每个正式点前先用原生 EXT4、128 KiB direct write
创建低碎片文件。随后卸载并按被测模式重新挂载，因此 overwrite 不包含首次 SSD
块分配，cold randread 不命中上一次挂载的页缓存。append 从空文件开始，包含首次
数据块和元数据分配。

图片只展示 fio 测量期带宽、平均延迟与 P99 延迟；写入点在 fio 后仍执行 `sync`，
包含该阶段的持久化带宽保存在 CSV 的 `including_sync_mib_s_*` 字段中。

## 关键带宽结果

以下均为三轮 fio 测量期均值，单位 MiB/s。

### 4 KiB thread sweep

| 设备 / 负载 | 模式 | 1 线程 | 4 线程 | 16 线程 | 32 线程 |
|---|---|---:|---:|---:|---:|
| NVMe overwrite | EXT4 | 270.5 | 324.4 | 358.4 | 292.6 |
| NVMe overwrite | rNVPCe | 188.2 | 603.3 | 555.9 | 551.2 |
| NVMe append | EXT4 | 727.8 | 1653.9 | 592.9 | 333.7 |
| NVMe append | rNVPCe | 198.4 | 265.7 | 420.8 | 275.7 |
| NVMe cold read | EXT4 | 36.3 | 132.9 | 407.1 | 243.0 |
| NVMe cold read | rNVPCe | 36.9 | 137.4 | 456.7 | 440.9 |
| SATA overwrite | EXT4 | 179.9 | 232.7 | 229.7 | 241.0 |
| SATA overwrite | rNVPCe | 202.1 | 304.3 | 304.6 | 300.4 |
| SATA append | EXT4 | 332.1 | 385.7 | 398.1 | 321.3 |
| SATA append | rNVPCe | 171.9 | 303.8 | 447.0 | 276.1 |
| SATA cold read | EXT4 | 20.9 | 65.9 | 154.2 | 183.7 |
| SATA cold read | rNVPCe | 23.5 | 66.1 | 189.7 | 250.3 |

NVMe cold read 在 20 线程以上有明显轮间波动，但 rNVPCe 与 EXT4 都能在较低
线程数随并发增长；这与大块单请求读的固定带宽形成鲜明对比。

### 单线程 bs sweep

| 设备 / 负载 | 模式 | 4 KiB | 128 KiB | 1 MiB |
|---|---|---:|---:|---:|
| NVMe overwrite | EXT4 | 198.4 | 998.8 | 924.1 |
| NVMe overwrite | rNVPCe | 185.1 | 2581.2 | 3087.1 |
| NVMe append | EXT4 | 699.4 | 1089.7 | 1103.9 |
| NVMe append | rNVPCe | 197.6 | 2530.4 | 3102.4 |
| NVMe cold read | EXT4 | 35.7 | 418.2 | 374.8 |
| NVMe cold read | rNVPCe | 36.5 | 36.3 | 41.8 |
| SATA overwrite | EXT4 | 177.6 | 340.3 | 343.0 |
| SATA overwrite | rNVPCe | 192.1 | 486.6 | 483.6 |
| SATA append | EXT4 | 296.5 | 361.2 | 336.3 |
| SATA append | rNVPCe | 200.2 | 486.5 | 482.6 |
| SATA cold read | EXT4 | 21.1 | 77.5 | 107.1 |
| SATA cold read | rNVPCe | 24.5 | 32.8 | 49.7 |

NVMe EXT4 在 128 KiB 及以上的轮间标准差可达均值的 40%--52%，因此表中
只用于观察数量级；rNVPCe 大块写和大块冷读的三轮结果反而较稳定。

## 写后同步

写后 `sync` 会降低两种模式的端到端带宽，但不改变主要排序。例如 NVMe、
128 KiB、单线程 overwrite 的 fio/含 sync 带宽分别为：EXT4 999/817 MiB/s，
rNVPCe 2581/1820 MiB/s；SATA 对应为 EXT4 340/316 MiB/s，rNVPCe
487/445 MiB/s。4 KiB append 的首次分配短板在包含 sync 后仍然存在。

## 结果图

- [Overwrite 带宽：thread sweep](../test_results/full-e2e-20261117/overwrite-randwrite-threads-bandwidth.png)
- [Overwrite 带宽：bs sweep](../test_results/full-e2e-20261117/overwrite-randwrite-bs-bandwidth.png)
- [Append 带宽：thread sweep](../test_results/full-e2e-20261117/append-write-threads-bandwidth.png)
- [Append 带宽：bs sweep](../test_results/full-e2e-20261117/append-write-bs-bandwidth.png)
- [Cold randread 带宽：thread sweep](../test_results/full-e2e-20261117/cold-randread-threads-bandwidth.png)
- [Cold randread 带宽：bs sweep](../test_results/full-e2e-20261117/cold-randread-bs-bandwidth.png)

同目录还包含上述六组实验的平均延迟和 P99 图。图中实线是三轮均值，每个 `X`
是单轮原始值。

## 数据与复现

- 复现说明：[README.md](../test_results/full-e2e-20261117/README.md)
- 原始逐轮数据：[results.csv](../test_results/full-e2e-20261117/results.csv)
- 三轮聚合数据：[aggregate.csv](../test_results/full-e2e-20261117/aggregate.csv)
- 测试与聚合脚本：[full-e2e-sweep-20261117](../scripts/full-e2e-sweep-20261117/README.md)

冻结代码为 `cce2180a2bdf324f9032633800a5f0eb6cd6e186` 加未提交差异，差异
SHA-256 为 `02d20245a177133b818a9ae5fcd5a462003fdc73b183c511c334737cfe2a5fbf`；
release ISO SHA-256 为
`d4ce0ac132c2031b8ed31b2449a80d377791dc385b804fe14789d3612441784b`。

外部完整运行证据保存在
`/home/zn/asterinas-build-env/full-e2e-20261117/results/`。正式结果目录共 648 个；
早期 runner 预检和被人工终止的 WAL 诊断目录使用 `failed-*` 前缀，不进入聚合。
