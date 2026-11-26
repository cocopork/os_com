# Append / overwrite / cold-read thread sweep 重测（2026-11-24）

本实验用于复查旧 full-e2e thread sweep 中星绽 EXT4 和 rNVPCe-SU 在高线程下
低于 Linux NVLog，且带宽走势异常的问题。本次不包含 bs sweep。

## 配置

- 设备：NVMe `/dev/nvme1n1p2`（8 virtqueue）；SATA `/dev/sda4`（1 virtqueue）。
- 对象：星绽 EXT4、星绽 rNVPCe-SU、Linux NVLog。
- jobs：1/4/8/12/16/20/24/28/32；4 KiB；`psync`；`iodepth=1`；每点3轮。
- 写入持久性：EXT4 保留 `fsync=0` 异步结果；rNVPCe-SU 与 NVLog 使用
  `fsync=1`，即每个 4 KiB 写后执行一次 `fsync()`。
- append：每 job 独占空文件并单遍写入 1 GiB；总写入量随 jobs 线性增长。
- overwrite：每 job 独占预置的低碎片 1 GiB 文件，执行一遍 4 KiB random
  overwrite；计量期不分配新的数据块，总工作集随 jobs 线性增长。
- cold randread：每点固定总数据集至少 2560 MiB，各 job 独占文件，
  `direct=0`、`norandommap=0`、`time_based=0`，每轮重新格式化、预生成并重新挂载。
- 缓存配额：DRAM PageCache 128 MiB + NVM data cache 128 MiB。

被测星绽代码基础提交为 `92d3f7269754f2a3b1e8ff5beb77c26688f94152`，镜像及
NVLog 启动文件哈希为：

```text
7b444d24e754ec5e7effe40f0fcaa267c4e5a5c42171acdd9aaa7e225ad8eedd  NVMe selected.iso
57ca4551f2bb565c116e95f97b452cbf629daba1ae665bc9007e908d72bce04d  SATA selected.iso
06ea2dcca70d690e5ec99f10d5e68dff57827144e7f33cb354cb3f6a2eef16ba  NVLog bzImage
26957baf223320780a861e426ab56059228e7f0c72aa0ddd7e58c08c1ba70877  NVLog rootfs-v5.ext4
```

append 最终改为固定每文件工作量，而非 `time_based` 循环。原因是顺序写在
有限文件上循环会回绕为 overwrite；而 NVLog 在超大稀疏文件上到达压力点后，
单个同步write可长时间阻塞，fio不能在30秒边界及时收尾。

## 带宽结果

下表为3轮均值，单位 MiB/s。

| 设备/负载 | 系统 | 1 job | 4 jobs | 8 jobs | 16 jobs | 24 jobs | 32 jobs |
|---|---|---:|---:|---:|---:|---:|---:|
| NVMe append | EXT4 | 276.2 | 306.9 | 303.6 | 522.5 | 602.1 | 494.6 |
| NVMe append | rNVPCe-SU | 246.4 | 226.9 | 367.5 | 426.7 | 329.6 | 272.6 |
| NVMe append | NVLog | 10.6 | 28.9 | 50.8 | 79.1 | 90.6 | 108.8 |
| NVMe overwrite | EXT4 | 133.0 | 155.1 | 179.3 | 207.8 | 248.2 | 264.3 |
| NVMe overwrite | rNVPCe-SU | 341.7 | 770.8 | 798.7 | 782.8 | 772.7 | 782.3 |
| NVMe overwrite | NVLog | 27.5 | 83.1 | 145.3 | 272.8 | 344.6 | 354.2 |
| NVMe cold read | EXT4 | 35.6 | 106.7 | 159.2 | 192.2 | 180.2 | 157.9 |
| NVMe cold read | rNVPCe-SU | 34.5 | 100.6 | 162.0 | 198.4 | 165.2 | 153.2 |
| NVMe cold read | NVLog | 28.2 | 124.4 | 248.1 | 442.5 | 534.2 | 530.1 |
| SATA append | EXT4 | 173.8 | 193.8 | 201.5 | 260.5 | 277.2 | 274.9 |
| SATA append | rNVPCe-SU | 234.5 | 183.4 | 196.6 | 195.5 | 198.9 | 180.0 |
| SATA append | NVLog | 8.4 | 15.9 | 24.7 | 35.4 | 45.8 | 50.8 |
| SATA overwrite | EXT4 | 127.9 | 131.0 | 150.8 | 184.2 | 196.7 | 197.3 |
| SATA overwrite | rNVPCe-SU | 205.2 | 191.3 | 185.1 | 193.0 | 194.9 | 192.7 |
| SATA overwrite | NVLog | 15.6 | 33.1 | 55.5 | 82.0 | 98.5 | 114.5 |
| SATA cold read | EXT4 | 21.3 | 64.3 | 87.1 | 90.6 | 93.5 | 90.2 |
| SATA cold read | rNVPCe-SU | 22.7 | 55.2 | 78.4 | 94.3 | 101.7 | 99.6 |
| SATA cold read | NVLog | 14.6 | 63.6 | 111.8 | 174.5 | 218.3 | 249.5 |

## 结论

1. NVLog 改为逐 4 KiB `fsync()` 并确认使用 NVM 日志后，append 不再呈现原先的
   1--2 GiB/s 异步带宽；NVMe 32 jobs 为108.8 MiB/s，SATA为50.8 MiB/s。
2. rNVPCe-SU 同步 overwrite 在 NVMe 4--32 jobs 保持约771--799 MiB/s，说明该路径
   在4 jobs后没有随线程数明显劣化；同步 append 则在16 jobs后由426.7 MiB/s降至
   272.6 MiB/s，仍有高并发瓶颈。
3. EXT4 保留 `fsync=0`，只表示异步 buffered-write 上限。它不能与两条同步写曲线
   直接做等持久性排名。
4. cold read 未重测：NVLog 在 NVMe/SATA 上均随 jobs 扩展，星绽
   EXT4/rNVPCe-SU 在12–16 jobs后基本饱和或回落。这与文件工作量缩小无关，
   后续应继续定位星绽 buffered-read/PageCache、块层提交和完成路径的并发上限。

第一轮 `e2e-sata-ext4-append-write-threads-reconfigured-24-r1` 首次执行后
e2fsck 报告 block bitmap 漏标和 group bitmap checksum 错误，状态4。原样重跑后
通过，正式数据使用重跑值；失败现场保存在外部实验目录。

## 文件

- `raw/*/result.json`：486份最终通过验证的逐轮结果。
- `results.csv`：统一绘图输入。
- `aggregate.csv`：162个坐标的3轮均值、标准差和范围。
- `plots/bandwidth-thread-summary.png`：两设备×三负载的带宽总图。
- `scripts/aggregate.py`、`scripts/plot.py`：可重复生成CSV与图片。

外部完整QEMU、fio、mkfs和e2fsck日志位于：

```text
/home/zn/asterinas-build-env/full-e2e-reconfigured-thread-20261124/
/home/zn/asterinas-build-env/full-e2e-sync-write-20261125/
/home/zn/asterinas-build-env/full-e2e-nvlog-sync-write-20261125/
```
