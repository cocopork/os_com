# 星绽 EXT4：SATA / NVMe direct I/O 扫描（2026-10-18）

完成两设备 × 两种扫描 × 9 个取值 × 4 种负载，共 **144 个测试点**。
所有 fio job 返回 error=0、完成计划字节数；两次 guest 卸载后 e2fsck -fn 均退出 0。

## 实验配置

- 当前内核基线：`93c2b45dd4ac49b14daab7e5c4ecaab7af61d286`，正常 release，无临时队列/延迟插桩。
- ISO SHA256：`72eff1d3ee81c72281f7529b4e6c06c2a5a21f7a75a209bae120182414427075`。
- SATA：Intel `SSDSC2KG03`，serial `PHYJ404101QE3P8DGN`，分区 `/dev/sda4`；
  ATA SSD 经 LSI SAS38xx HBA 接入（udev 有 scsi-SATA 标识，lsblk TRAN 显示 sas）。
- NVMe：DapuStor `DPRP5104T0TG03T8000`，serial `RS6U01A26403014R`，分区 `/dev/nvme1n1p1`。
- 两个分区均经授权重新格式化为 4KiB-block journal EXT4；delalloc 开启；rNVPC/rNVPCe/rTIO 关闭。
- fio 3.39：`ioengine=psync direct=1 iodepth=1 thread=1 fsync=0 group_reporting=1`。
- thread sweep：1,4,8,12,16,20,24,28,32，固定 bs=4KiB。
- bs sweep：4,8,16,32,64,128,256,512,1024KiB，固定 numjobs=1。
- 每线程独立 inode，实际顺序写满文件并 sync 后测覆盖写/读；每点一遍约 2GiB，
  单线程大小 `floor(2048/jobs)` MiB，因此总量 2040–2048MiB。600秒仅为异常上限，未采用 time_based。
- 64 vCPU / 8GiB，fio split 绑定 CPU0–31，普通内核32–59，writeback60，预留61，virtio提交62，journal63。
  串口日志验证 virtio=62、journal=63；未对主机 QEMU 线程做 CPU/NUMA pinning。
- 相同 QEMU/KVM、virtio-blk 单队列、queue-size=1024、indirect_desc=off、cache=none、aio=threads。
  两块盘串行测试。测量包含 guest EXT4、virtio、QEMU 和物理设备路径，并非星绽原生 AHCI/NVMe 驱动比较。
- 主机两设备 max_sectors_kb 均为128，故大块 guest I/O 可能在主机拆分；SATA mq-deadline/nr_requests=256，
  NVMe none/nr_requests=1023，均保持现有默认值。主机 sysfs 报告 SATA write back、NVMe write through，未修改缓存策略。

## 带宽对比

单位 MiB/s。全部 IOPS、平均完成延迟、p99 完成延迟、运行时间和字节数见 [DEVICE-DIRECT-SWEEP-2026-10-18.csv](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18.csv)。

| 配置 | workload | SATA | NVMe | NVMe/SATA |
|---|---|---:|---:|---:|
| 4KiB / 1线程 | randwrite | 35.7 | 43.2 | 1.21× |
| 4KiB / 1线程 | randread | 24.3 | 28.3 | 1.16× |
| 4KiB / 1线程 | write | 36.8 | 42.7 | 1.16× |
| 4KiB / 1线程 | read | 38.5 | 45.8 | 1.19× |
| 4KiB / 32线程 | randwrite | 284.6 | 334.5 | 1.18× |
| 4KiB / 32线程 | randread | 284.9 | 301.0 | 1.06× |
| 4KiB / 32线程 | write | 287.3 | 389.4 | 1.36× |
| 4KiB / 32线程 | read | 287.5 | 309.3 | 1.08× |
| 128KiB / 1线程 | randwrite | 247.1 | 840.0 | 3.40× |
| 128KiB / 1线程 | randread | 97.8 | 466.5 | 4.77× |
| 128KiB / 1线程 | write | 246.9 | 851.6 | 3.45× |
| 128KiB / 1线程 | read | 228.6 | 905.8 | 3.96× |
| 1MiB / 1线程 | randwrite | 376.9 | 2013.8 | 5.34× |
| 1MiB / 1线程 | randread | 298.2 | 1884.1 | 6.32× |
| 1MiB / 1线程 | write | 398.5 | 1738.5 | 4.36× |
| 1MiB / 1线程 | read | 435.8 | 2343.2 | 5.38× |

## 曲线

### Thread sweep：4KiB

![Thread sweep：4KiB](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-threads-mib_s.png)

[IOPS 曲线](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-threads-iops.png)，[带宽 SVG](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-threads-mib_s.svg)。

### Block-size sweep：1线程

![Block-size sweep：1线程](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-bs-mib_s.png)

[IOPS 曲线](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-bs-iops.png)，[带宽 SVG](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/DEVICE-DIRECT-SWEEP-2026-10-18-bs-mib_s.svg)。

## 结论与范围

- 小块单线程时两设备差距较小；增加线程后 NVMe 能达到更高的4KiB吞吐。
- 本轮 NVMe 高线程曲线有明显波动，32线程不是最高带宽点；SATA 的4KiB吞吐在高线程下逐渐趋近285–288MiB/s。
- 增大单次 I/O 后设备差距更明显。两组扫描本身不能进一步归因于特定锁、提交线程或设备队列，需要独立的延迟分解。
- 每点仅一轮2GiB，未做长时稳态/全盘预处理或统计重复；部分大块点只有约1秒，数值适合首轮趋势观察。
  同配置的 thread=1/4KiB 和 bs=4KiB/1线程重复点也保留在 CSV，可观察短测波动。
- direct=1 绕过 guest page cache；fsync=0 不等于断电持久化，也不绕过设备内部写缓存。
  fio 带宽不含每点后单独执行的 sync 时间。

## 复现与原始证据

脚本：[`docs/os_com_implementation/test/device-direct-sweep`](../../os_com_implementation/test/device-direct-sweep/README.md)。
保留原始数据目录：`/home/zn/asterinas-build-env/device-direct-sweep-20261018/`，含两设备串口日志、
ISO、manifest、环境信息、e2fsck输出、CSV、PNG/SVG。

```sh
python3 docs/os_com_implementation/test/device-direct-sweep/summarize.py \
  /home/zn/asterinas-build-env/device-direct-sweep-20261018 --require-complete
```

本轮只新增测试脚本及结果文档，未修改内核代码，未提交或推送。
