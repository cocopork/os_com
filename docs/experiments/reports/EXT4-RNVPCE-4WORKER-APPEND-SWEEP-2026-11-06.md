# EXT4 writeback 与 rNVPCe 四 worker 首次追加写扫描

## 结论

2026-11-09 在 `/dev/nvme1n1p2` 上重新执行 36 个 append 测试点，每点 5 轮，
共 180 个有效样本。被测文件在计时前不存在，因此结果包含文件创建、数据块、
bitmap/GDT 和 extent 等首次分配与更新开销。曲线为 fio 测试区间带宽的算术
平均值，`X` 为全部单轮样本。

- 单线程 4 KiB 时两者接近：EXT4 `186.54 MiB/s`，rNVPCe
  `194.02 MiB/s`。
- rNVPCe 的首次写路径没有随线程扩展。其带宽从 8 jobs 的
  `264.93 MiB/s` 持续降至 32 jobs 的 `142.62 MiB/s`，平均延迟同时从
  `108.46 us` 增至 `823.00 us`；32 jobs 只有 EXT4 的 `0.45x`。
- 最终 SSD extent 数不能单独解释该退化：24--32 jobs 时 rNVPCe 文件通常只有
  1 个 extent，高线程回退仍在五轮中稳定复现。
- bs sweep 能从范围事务受益。8 KiB 起 rNVPCe 超过 EXT4，128--1024 KiB
  平均达到 `1.62--1.86 GiB/s`，但大请求单轮结果仍有明显波动。

![Thread sweep](../test_result_images/ext4-rnvpce-4worker-append-thread-sweep.png)

![Block-size sweep](../test_result_images/ext4-rnvpce-4worker-append-bs-sweep.png)

## 测试口径

| 项目 | 配置 |
| --- | --- |
| 设备 | `/dev/nvme1n1p2`，每点重新格式化 |
| 重复次数 | 每个点 5 轮；算术平均，保留全部原始样本 |
| 数据量 | 每点约 2 GiB；多 job 时每个 job 使用独立新 inode |
| I/O | fio `psync`、buffered I/O、顺序首次写入、`fallocate=none` |
| thread sweep | 4 KiB；jobs = 1, 4, 8, 12, 16, 20, 24, 28, 32 |
| bs sweep | 1 job；4, 8, 16, 32, 64, 128, 256, 512, 1024 KiB |
| EXT4 | journal、delalloc、writeback；rNVPC/rNVPCe/rTIO 关闭 |
| rNVPCe | nojournal、rNVPCe + rTIO；4 个 inode-owned worker |
| 块设备队列 | 两种模式均使用 4 个 virtqueue/IOThread |
| 正确性 | 精确字节数、fio、sync、umount、e2fsck 和最终布局逐点校验 |

## 4 KiB thread sweep

| jobs | EXT4 fio | rNVPCe fio | 比值 | EXT4 平均延迟 | rNVPCe 平均延迟 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 186.54 | 194.02 | 1.04x | 33.14 us | 19.95 us |
| 4 | 418.29 | 194.82 | 0.47x | 39.51 us | 75.19 us |
| 8 | 522.70 | 264.93 | 0.51x | 60.19 us | 108.46 us |
| 12 | 435.19 | 257.65 | 0.59x | 111.32 us | 176.63 us |
| 16 | 426.29 | 242.18 | 0.57x | 139.98 us | 251.26 us |
| 20 | 425.44 | 205.75 | 0.48x | 177.13 us | 370.97 us |
| 24 | 391.51 | 182.17 | 0.47x | 232.93 us | 494.88 us |
| 28 | 335.18 | 160.23 | 0.48x | 307.68 us | 657.52 us |
| 32 | 314.30 | 142.62 | 0.45x | 382.53 us | 823.00 us |

## bs sweep

| bs (KiB) | EXT4 fio | rNVPCe fio | 比值 | EXT4 平均延迟 | rNVPCe 平均延迟 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 206.03 | 185.03 | 0.90x | 19.34 us | 20.86 us |
| 8 | 281.79 | 351.36 | 1.25x | 27.60 us | 21.98 us |
| 16 | 368.33 | 621.05 | 1.69x | 42.30 us | 24.94 us |
| 32 | 408.17 | 1033.47 | 2.53x | 76.50 us | 30.14 us |
| 64 | 383.56 | 1430.21 | 3.73x | 165.11 us | 44.37 us |
| 128 | 365.22 | 1803.06 | 4.94x | 346.97 us | 69.60 us |
| 256 | 349.89 | 1860.74 | 5.32x | 716.83 us | 138.55 us |
| 512 | 363.21 | 1622.61 | 4.47x | 1412.96 us | 312.03 us |
| 1024 | 279.13 | 1734.22 | 6.21x | 3597.90 us | 581.95 us |

带宽单位均为 MiB/s。图中只展示 fio 测试期间带宽，不展示测试后的全局
`sync` 带宽。

## 完整性与产物

五轮使用同一 Asterinas 镜像，SHA-256 为
`e807b2390b7ddee6d861bd3c2149a16b36cde083a0fca0a0d26d62f26c2eb0d0`。
180/180 个单轮样本均通过 runner、精确字节数、卸载和 e2fsck 校验。

- 测试原始目录：`/home/zn/asterinas-build-env/ext4-rnvpce-append-sweep-20261109-5x/round-{1,2,3,4,5}`
- 五轮原始数据：`docs/experiments/test_result_images/fio_test/ext4-rnvpce-4worker-append-repeats-5x.csv`
- 五轮均值数据：`docs/experiments/test_result_images/fio_test/ext4-rnvpce-4worker-append-sweep.csv`
- 图片：`docs/experiments/test_result_images/ext4-rnvpce-4worker-append-*.png`
- 测试与绘图脚本：`docs/experiments/scripts/ext4-rnvpce-append-sweep-20261106`
