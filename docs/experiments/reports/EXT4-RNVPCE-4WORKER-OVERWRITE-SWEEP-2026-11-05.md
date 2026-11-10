# EXT4 writeback 与 rNVPCe 四 worker 覆盖写扫描

## 结论

2026-11-09 在 `/dev/nvme1n1p2` 上重新执行完整 overwrite 矩阵：72 个测试点，
每点独立执行 5 轮，共 360 个有效样本。表格和曲线采用 fio 测试区间带宽的算术
平均值；图中的 `X` 保留全部单轮结果，不绘制包含测试后 `sync` 的带宽。

- 4 KiB 随机覆盖写从 4 jobs 的 `502.67 MiB/s` 开始进入平台，32 jobs 降至
  `289.93 MiB/s`；增加线程没有带来线性扩展。
- 4 KiB 顺序覆盖写单线程达到 `654.61 MiB/s`，4 jobs 降至
  `512.31 MiB/s`，8--16 jobs 约为 `608--641 MiB/s`，随后随线程数下降。
- bs sweep 明确受益于范围事务。随机写在 128 KiB 后约为
  `2.22--2.46 GiB/s`，顺序写在 16 KiB 后约为 `1.86--2.62 GiB/s`。
- 五轮结果仍有明显波动。例如 EXT4 512 KiB 顺序写有一轮仅
  `37.65 MiB/s`；该样本通过布局和文件系统校验，因而原样计入均值。

## 测试口径

| 项目 | 配置 |
| --- | --- |
| 设备 | `/dev/nvme1n1p2`，每个点重新格式化 |
| 重复次数 | 每个点 5 轮；算术平均，保留全部原始样本 |
| 数据量 | 每点约 2 GiB；多 job 时每个 job 使用独立 inode |
| I/O | fio `psync`、buffered I/O、overwrite |
| thread sweep | 4 KiB；jobs = 1, 4, 8, 12, 16, 20, 24, 28, 32 |
| bs sweep | 1 job；4, 8, 16, 32, 64, 128, 256, 512, 1024 KiB |
| EXT4 | journal、delalloc、writeback；rNVPC/rNVPCe/rTIO 关闭 |
| rNVPCe | nojournal、rNVPCe + rTIO；4 个 inode-owned worker |
| 块设备队列 | 两种模式均使用 4 个 virtqueue/IOThread |
| 文件布局 | 串行预创建低碎片文件；逐文件用 `debugfs stat` 验证 |
| 正确性 | fio、sync、umount、e2fsck、布局和 hctx 活动逐点校验 |

## 4 KiB thread sweep

| jobs | 随机 EXT4 | 随机 rNVPCe | 顺序 EXT4 | 顺序 rNVPCe |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 127.71 | 181.61 | 207.31 | 654.61 |
| 4 | 168.78 | 502.67 | 521.65 | 512.31 |
| 8 | 169.59 | 487.41 | 572.29 | 640.55 |
| 12 | 194.96 | 444.66 | 568.99 | 632.73 |
| 16 | 203.54 | 461.35 | 473.06 | 607.90 |
| 20 | 211.62 | 483.81 | 411.42 | 568.43 |
| 24 | 243.24 | 401.50 | 370.23 | 484.68 |
| 28 | 272.68 | 391.01 | 337.40 | 425.94 |
| 32 | 261.63 | 289.93 | 294.71 | 384.47 |

## bs sweep

| bs (KiB) | 随机 EXT4 | 随机 rNVPCe | 顺序 EXT4 | 顺序 rNVPCe |
| ---: | ---: | ---: | ---: | ---: |
| 4 | 134.25 | 191.91 | 206.84 | 652.86 |
| 8 | 184.75 | 370.61 | 291.55 | 1108.40 |
| 16 | 267.52 | 651.27 | 350.23 | 1860.16 |
| 32 | 343.40 | 1123.37 | 360.20 | 2042.35 |
| 64 | 344.96 | 1638.39 | 330.99 | 2359.71 |
| 128 | 360.96 | 2409.38 | 330.34 | 2621.35 |
| 256 | 387.41 | 2435.95 | 329.07 | 2136.19 |
| 512 | 437.06 | 2223.19 | 282.32 | 2284.13 |
| 1024 | 292.53 | 2460.26 | 241.68 | 2195.07 |

带宽单位均为 MiB/s。

## 完整性与产物

五轮使用同一 Asterinas 镜像，SHA-256 为
`e807b2390b7ddee6d861bd3c2149a16b36cde083a0fca0a0d26d62f26c2eb0d0`。
360/360 个单轮样本均通过 runner、精确字节数、卸载和 e2fsck 校验。

- 测试原始目录：`/home/zn/asterinas-build-env/ext4-rnvpce-overwrite-sweep-20261109-5x/round-{1,2,3,4,5}`
- 五轮原始数据：`docs/experiments/test_result_images/fio_test/ext4-rnvpce-4worker-overwrite-repeats-5x.csv`
- 五轮均值数据：`docs/experiments/test_result_images/fio_test/ext4-rnvpce-4worker-overwrite-sweep.csv`
- thread sweep 图：`docs/experiments/test_result_images/ext4-rnvpce-4worker-thread-sweep.png`
- bs sweep 图：`docs/experiments/test_result_images/ext4-rnvpce-4worker-bs-sweep.png`
- 测试与绘图脚本：`docs/experiments/scripts/ext4-rnvpce-overwrite-sweep-20261105`
