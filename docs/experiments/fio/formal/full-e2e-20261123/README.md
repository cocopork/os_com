# EXT4 / rNVPCe-SU / NVLog 完整端到端复测（2026-11-23）

本目录保存当前星绽代码与 Linux NVLog 在 NVMe、SATA 上的完整 fio 复测结果。测试坐标与
`full-e2e-20261117` 一致，并将设备队列改为 NVMe 8 virtqueue/8 IOThread、SATA 1
virtqueue/1 IOThread。

完整复现条件、安全检查和后续 append thread-sweep 修订契约见
[`protocol.md`](protocol.md)，执行及绘图脚本位于 [`scripts/`](scripts/)。

## 配置

- 对象：星绽 EXT4 journal、星绽 rNVPCe-SU + rTIO、Linux NVLog；
- 负载：4 KiB 随机覆盖写、顺序 append、buffered cold randread；
- thread sweep：4 KiB，1/4/8/12/16/20/24/28/32 jobs；
- bs sweep：1 job，4/8/16/32/64/128/256/512/1024 KiB；
- fio：`psync`、`iodepth=1`、每点约 2 GiB，每坐标重复 3 轮；
- VM：64 vCPU、8 GiB；NVMe 使用 8 virtqueue，SATA 使用 1 virtqueue；
- 星绽 NVM：128 MiB pool、64 MiB metadata reserve、64 MiB data limit；
- rTIO：NVMe 4 worker，SATA 1 worker；
- NVLog：258 MiB NVDIMM 镜像，`nvpc_pages=32768`。

### 2026-11-25 rNVPCe-SU / NVLog 同步写复测

rNVPCe-SU 与 NVLog 的 append-write 和 overwrite-randwrite 已改为 `fsync=1`：每个
fio block 写入后执行一次 `fsync()`。cold randread 保持 `fsync=0`；EXT4 不重测，
保留既有 `fsync=0` 数据作为异步对照。bs sweep 沿用每点 2 GiB；thread sweep 采用
每 job 独占 1 GiB 文件的修订矩阵。NVMe 使用 8 virtqueue，SATA 使用 1 virtqueue，
每坐标重复 3 轮。

两种同步写结果均记录 `fio_fsync_blocks=1`。解析器还要求每个 NVLog 写点的
fio 运行期间 `nvpcctl used pages` 峰值大于 0，并把峰值写入原始结果。EXT4 曲线与
rNVPCe-SU/NVLog 的持久化语义不同，只作为异步 buffered-write 上限对照。

星绽代码为提交 `92d3f7269754f2a3b1e8ff5beb77c26688f94152`，并启用
`virtio_blk_direct_submit=on`：

```text
d7ce7bd413d9555853240b800cb17a1417e17557ae0c09a5cc89d7ab2aff1995  NVMe selected.iso
8827fb508c3535928fe7fff48614ece200ee4963a75292e8717c585203ae5942  SATA selected.iso
06ea2dcca70d690e5ec99f10d5e68dff57827144e7f33cb354cb3f6a2eef16ba  NVLog bzImage
26957baf223320780a861e426ab56059228e7f0c72aa0ddd7e58c08c1ba70877  NVLog rootfs-v5.ext4
```

### 2026-11-24 append thread sweep 历史复测

本节记录 2026-11-24 的异步写历史复测，写数据已被 2026-11-25 同步写复测取代；
cold randread 和 EXT4 数据仍保留。该轮保持每点
约 2 GiB、三轮、NVMe 8 virtqueue/4 rTIO worker、SATA 1 virtqueue/1 rTIO worker。

源码基于提交 `92d3f7269754f2a3b1e8ff5beb77c26688f94152`，并包含 EXT4
reservation 分区及 delayed-promise 单锁收尾修改；三个相关源码文件的补丁 SHA-256
为 `0220bc0f2b6e4a3071083af21621715186c4a5ddc88faca4c85d9f165ca03f54`。

```text
7b444d24e754ec5e7effe40f0fcaa267c4e5a5c42171acdd9aaa7e225ad8eedd  NVMe selected.iso
57ca4551f2bb565c116e95f97b452cbf629daba1ae665bc9007e908d72bce04d  SATA selected.iso
```

复测确认 NVMe 高线程回退仍然存在：EXT4 从 20 jobs 的 1118.6 MiB/s 降至
32 jobs 的 636.2 MiB/s，下降 43.1%；rNVPCe-SU 从 12 jobs 的 525.2 MiB/s
降至 32 jobs 的 272.9 MiB/s，下降 48.0%。SATA EXT4 从 20 jobs 的
357.4 MiB/s 降至 32 jobs 的 304.2 MiB/s，下降 14.9%；SATA rNVPCe-SU
在 1--32 jobs 间主要维持 193--219 MiB/s，表现为不扩展，但没有稳定的高线程下降。

NVLog 代码提交为 `989e2da47c60b97d5a28c59bde7bbb37c6cf5d1f`：

```text
06ea2dcca70d690e5ec99f10d5e68dff57827144e7f33cb354cb3f6a2eef16ba  bzImage
c93b94ea3084a1ef70d22fb88873fd77bedd9899f241df57c607c2496583cf98  rootfs-v5.ext4
```

### 2026-11-24 thread sweep 修订矩阵

append、overwrite 和 cold randread 已按 `protocol.md` 第 2.1--2.3 节的新契约另行
重测。overwrite 改为每 job 独占预置的低碎片 1 GiB 文件，计量期执行一遍 4 KiB
random overwrite，总工作集随 jobs 线性增长。486 份逐轮结果、162 个聚合坐标和
2x3 总图保存在 `../../temporary/thread-sweep-reconfigured-20261124/`。本目录下的
full-e2e CSV 已覆盖 2026-11-25 的同步写 bs sweep，但保留历史 thread sweep；
修订 thread sweep 不与其混写，以临时实验目录中的 CSV 和总图为准。

## 关键结果

下表为三轮均值，单位为 MiB/s。thread sweep 数据来自修订矩阵；EXT4 写为
`fsync=0`，rNVPCe-SU/NVLog 写为 `fsync=1`。

| 设备 | 负载与坐标 | EXT4 | rNVPCe-SU | NVLog |
|---|---|---:|---:|---:|
| NVMe | overwrite，32 jobs | 264.3 | 782.3 | 354.2 |
| NVMe | append，32 jobs | 494.6 | 272.6 | 108.8 |
| NVMe | cold read，32 jobs | 157.9 | 153.2 | 530.1 |
| NVMe | overwrite，128 KiB | 1275.4 | 3074.4 | 256.9 |
| NVMe | append，128 KiB | 1475.3 | 2648.0 | 102.6 |
| NVMe | cold read，128 KiB | 672.7 | 527.2 | 327.9 |
| SATA | overwrite，32 jobs | 197.3 | 192.7 | 114.5 |
| SATA | append，32 jobs | 274.9 | 180.0 | 50.8 |
| SATA | cold read，32 jobs | 90.2 | 99.6 | 249.5 |
| SATA | overwrite，128 KiB | 371.4 | 412.7 | 111.6 |
| SATA | append，128 KiB | 385.0 | 442.3 | 71.0 |
| SATA | cold read，128 KiB | 92.8 | 73.3 | 79.9 |

本轮验证了原 NVLog 写曲线主要是异步底层文件系统写路径：改为每 block `fsync()`
并确认 NVLog 使用量后，NVMe 128 KiB append 从原 860.0 MiB/s 降为 102.6 MiB/s。
rNVPCe-SU 的同步覆盖写仍明显高于异步 EXT4 对照，但小块 append 和高并发 append
仍是主要弱项。由于 EXT4 没有采用相同 `fsync=1` 语义，不能据此进行等持久性排名。

## 一致性异常

第三轮 `e2e-nvme-rnvpce-cold-randread-threads-28-r3` 首次运行中，一个 fio 线程收到
`EIO`，后置 e2fsck 报告块位图漏标 `388512--407199`，状态为 4。失败现场保存在：

```text
/home/zn/asterinas-build-env/full-e2e-20261123/failed/
failed-e2e-nvme-rnvpce-cold-randread-threads-28-r3-attempt1/
```

完全相同配置原样重跑后 fio 和 e2fsck 均通过，正式矩阵采用重跑结果。该现象应视为
一次未稳定复现的高并发准备/读取路径一致性缺陷，不能解释为性能波动。

## 数据

- `results.csv`：972 次逐轮结果；
- `aggregate.csv`：324 个坐标的均值、标准差、最小值和最大值；
- `nvlog-results.csv`：324 次 NVLog 逐轮结果；
- `raw/asterinas/*/result.json`：648 次 EXT4/rNVPCe 结果；
- `raw/nvlog/*/result.json`：324 次 NVLog 结果；
- `plots/`：`bandwidth-bs-summary.png` 和 `bandwidth-thread-summary.png`。

2026-11-24 append thread sweep 的完整 QEMU、fio、格式化和 e2fsck 日志保存在：

```text
/home/zn/asterinas-build-env/full-e2e-append-thread-retest-20261124/
```

2026-11-25 同步写复测日志保存在：

```text
/home/zn/asterinas-build-env/full-e2e-sync-write-20261125/
/home/zn/asterinas-build-env/full-e2e-nvlog-sync-write-20261125/
```

完整 QEMU、fio、格式化和 e2fsck 日志保存在：

```text
/home/zn/asterinas-build-env/full-e2e-20261123/
```
