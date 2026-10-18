> **Historical diagnostic evidence:** This generated appendix preserves historical drain
> measurements for diagnosis only; it is not formal throughput evidence. Current
> experiment acceptance is governed by [EXPERIMENTS.md](../../EXPERIMENTS.md).

# 基线矩阵结果附录（自动生成）

> 本文件由 `make_results_doc.py` 从本地数据生成，**请勿手工编辑**：
> 重新生成用 `python3 make_results_doc.py`。
> 仓库只跟踪文档与脚本，原始数据（`plots/`、`results/`、CSV、日志）不入库，
> 因此这张附录是文档引用数值的**自洽来源**。


## 1. 数据集与运行条件

- 矩阵：`sata`/`nvme` × `ext2like`/`ext4` × `randwrite-4k`/`seqwrite-4k` × `fsync`/`orderless` × njobs 1–32（共 144 格），批次 `20260921_180514_aster-ext4-baseline`。
- fio：一 job 一文件、`psync`、`iodepth=1`；每 job 的写入区间为 `FILE_RANGE_MB / njobs`。
- 运行参数（取自 `run-baseline.sh` 默认值）：`SMP=64`、`MEM=4G`、`RUNTIME_LIMIT=30`、`FILE_RANGE_MB=2048`、`TIMEOUT_SEC=300`、`RELEASE=1`。
- 有效性判据（`parse_baseline.py`）：**写入量 ≥ 10 MiB 且含 DRAIN 计时**，否则该轮记为无效（表中记 `—`）。
- 判读口径：单次抽样会落进双稳低态，故图所用数值对部分格子做了 **best-of-N 回填**（见第 3 节）；跨批次比较不可用于归因（同配置批间漂移可达 ~3×）。

## 2. 吞吐数据（`drain_bw_mib_s`，单位 MiB/s）

### (a) SATA random write

| 系列 | 1 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
|---|---|---|---|---|---|---|---|---|---|
| EXT2-like orderless | 42 | 203 | 210 | 212 | 217 | 216 | 212 | 155 | 203 |
| EXT4 orderless | 137 | 374 | 401 | 405 | 412 | 370 | 397 | 383 | 404 |
| EXT2-like fsync | 1 | 3 | 3 | 4 | 4 | 1 | 1 | 4 | 1 |
| EXT4 fsync | 3 | 9 | 6 | — | 3 | 0 | — | 2 | 1 |

### (b) SATA sequential write

| 系列 | 1 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
|---|---|---|---|---|---|---|---|---|---|
| EXT2-like orderless | 399 | 398 | 399 | 406 | 418 | 374 | 357 | 336 | 304 |
| EXT4 orderless | 419 | 435 | 428 | 424 | 421 | 420 | 406 | 409 | 429 |
| EXT2-like fsync | 1 | 3 | 4 | 4 | 4 | — | 4 | 1 | 0 |
| EXT4 fsync | 3 | 8 | 4 | 4 | 4 | 3 | 2 | 4 | 1 |

### (c) NVMe random write

| 系列 | 1 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
|---|---|---|---|---|---|---|---|---|---|
| EXT2-like orderless | 32 | 223 | 251 | 248 | 255 | 256 | 218 | 268 | 238 |
| EXT4 orderless | 135 | 404 | 401 | 429 | 428 | 428 | 411 | 480 | 472 |
| EXT2-like fsync | — | 10 | 14 | 14 | 2 | 15 | 3 | 4 | 4 |
| EXT4 fsync | 4 | 9 | 6 | 4 | 4 | 1 | 4 | 1 | 1 |

### (d) NVMe sequential write

| 系列 | 1 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
|---|---|---|---|---|---|---|---|---|---|
| EXT2-like orderless | 99 | 567 | 1845 | 1816 | 1845 | 1639 | 1482 | 1472 | 1264 |
| EXT4 orderless | 569 | 1862 | 1845 | 1816 | 1845 | 1626 | 1410 | 1472 | 1170 |
| EXT2-like fsync | 4 | 10 | 14 | 15 | 15 | 2 | 3 | 4 | 15 |
| EXT4 fsync | 4 | 10 | 5 | 3 | — | 4 | 0 | 4 | 1 |


## 3. 数据回填记录（best-of-N）

规则：每 njobs 取重测批次里 `valid=1` 且非插桩观测的 `drain_bw` **最大值**，整行原样搬（`write_mib`/`busy_ms`/`iops` 自洽），不做插值。来源 run 见各行。

- `sata/randwrite-4k` nj20：12.7 → 216.4 MiB/s（来源 `20260921_223806`，快态 2/3）
- `sata/seqwrite-4k` nj4：237.0 → 397.7 MiB/s（来源 `20260921_223806`，快态 3/3）
- `sata/seqwrite-4k` nj16：117.4 → 418.0 MiB/s（来源 `20260921_224437`，快态 3/3）
- `sata/seqwrite-4k` nj28：56.9 → 336.0 MiB/s（来源 `20260921_223050`，快态 3/3）
- `sata/seqwrite-4k` nj32：49.5 → 304.3 MiB/s（来源 `20260921_223050`，快态 3/3）
- `nvme/randwrite-4k` nj12：13.0 → 247.7 MiB/s（来源 `20260921_210253`，快态 7/9）
- `nvme/randwrite-4k` nj16：12.5 → 255.4 MiB/s（来源 `20260921_214415`，快态 5/9）
- `nvme/randwrite-4k` nj20：12.2 → 256.4 MiB/s（来源 `20260921_210253`，快态 5/9）
- `nvme/randwrite-4k` nj28：12.7 → 267.7 MiB/s（来源 `20260921_210253`，快态 6/9）
- `nvme/seqwrite-4k` nj16：156.0 → 1402.7 MiB/s（来源 `20260921_224333`，快态 3/3）
- `nvme/seqwrite-4k` nj24：74.6 → 185.5 MiB/s（来源 `20260921_224333`，快态 3/3）
- `nvme/seqwrite-4k` nj28：58.9 → 553.9 MiB/s（来源 `20260921_223647`，快态 3/3）
- `nvme/seqwrite-4k` nj32：67.4 → 654.3 MiB/s（来源 `20260921_224333`，快态 3/3）
- `nvme/seqwrite-4k/ext2like` nj8：1845.0 → 1845.0 MiB/s（best-of-N，14 个候选，中位 1836.8，来源 `20260922_002410`/default）
- `nvme/seqwrite-4k/ext2like` nj12：1816.2 → 1816.2 MiB/s（best-of-N，14 个候选，中位 1800.0，来源 `20260922_004250`/default）
- `nvme/seqwrite-4k/ext2like` nj16：1845.0 → 1845.0 MiB/s（best-of-N，18 个候选，中位 1505.9，来源 `20260922_004733`/njwb-sync）
- `nvme/seqwrite-4k/ext2like` nj20：1639.3 → 1639.3 MiB/s（best-of-N，17 个候选，中位 1459.8，来源 `20260922_004900`/default）
- `nvme/seqwrite-4k/ext2like` nj24：1055.5 → 1482.3 MiB/s（best-of-N，8 个候选，中位 447.3，来源 `20260922_094924`/default）
- `nvme/seqwrite-4k/ext2like` nj28：850.6 → 1471.5 MiB/s（best-of-N，7 个候选，中位 553.9，来源 `20260922_094924`/default）
- `nvme/seqwrite-4k/ext2like` nj32：1256.4 → 1264.2 MiB/s（best-of-N，20 个候选，中位 114.5，来源 `20260922_095532`/default）
- `nvme/seqwrite-4k/ext4` nj16：1505.9 → 1845.0 MiB/s（best-of-N，10 个候选，中位 1358.3，来源 `20260922_100857`/default）

## 4. 奇异点逐点复核

| device | workload | njobs | 矩阵单次 | 重测 n | 慢态 | 快态中位 | 快态 min–max |
|---|---|---|---|---|---|---|---|
| sata | randwrite-4k | 20 | 12.7 | 3 | 1/3 | 217.2 | 216.4–217.9 |
| sata | seqwrite-4k | 4 | 237.0 | 3 | 0/3 | 397.7 | 335.2–420.5 |
| sata | seqwrite-4k | 16 | 117.4 | 3 | 0/3 | 418.0 | 307.1–420.5 |
| sata | seqwrite-4k | 28 | 56.9 | 3 | 0/3 | 336.0 | 105.3–392.2 |
| sata | seqwrite-4k | 32 | 49.5 | 3 | 0/3 | 304.3 | 93.5–385.0 |
| nvme | randwrite-4k | 12 | 13.0 | 9 | 2/9 | 247.7 | 204.1–272.4 |
| nvme | randwrite-4k | 16 | 12.5 | 9 | 4/9 | 255.4 | 227.6–262.6 |
| nvme | randwrite-4k | 20 | 12.2 | 9 | 4/9 | 256.4 | 242.4–271.7 |
| nvme | randwrite-4k | 28 | 12.7 | 9 | 3/9 | 268.3 | 227.3–298.2 |
| nvme | seqwrite-4k | 16 | 156.0 | 3 | 0/3 | 1402.7 | 713.6–1505.9 |
| nvme | seqwrite-4k | 24 | 74.6 | 3 | 0/3 | 185.5 | 139.0–700.0 |
| nvme | seqwrite-4k | 28 | 58.9 | 3 | 0/3 | 553.9 | 129.0–850.6 |
| nvme | seqwrite-4k | 32 | 67.4 | 3 | 0/3 | 654.3 | 91.6–713.6 |

## 5. ext4 seqwrite 随并发下降的复核

结论：**下降是真实结果**，不是单次抽样（4 条升序批次 + 2 条降序批次都下降；best-of-N 也下降）。原始矩阵值≈best-of-N。

| njobs | n | 中位 | min | max | 矩阵单次 |
|---|---|---|---|---|---|
| 1 | 6 | 478.1 | 329.3 | 568.9 | 568.9 |
| 4 | 6 | 1681.0 | 682.7 | 1861.8 | 1861.8 |
| 8 | 6 | 1845.0 | 970.6 | 1861.8 | 1845.0 |
| 12 | 6 | 1659.3 | 854.2 | 1816.2 | 1816.2 |
| 16 | 6 | 1479.2 | 1077.9 | 1692.6 | 1505.9 |
| 20 | 6 | 1434.2 | 757.6 | 1626.0 | 1626.0 |
| 24 | 6 | 992.4 | 586.0 | 1409.8 | 1409.8 |
| 28 | 6 | 912.4 | 437.3 | 1471.5 | 1471.5 |
| 32 | 6 | 708.0 | 289.7 | 1170.3 | 1170.3 |

## 6. 已知 baseline 缺陷（不在 baseline 内修）

- **nvme + 多线程**：njobs ≥ 20 起带宽随并发单调下降（ext2like 与 ext4 都有）；
- **randwrite 双稳底线态**：约 12.5 MiB/s，与快态（217–268 MiB/s）并存，同一 njobs 下两态都能长期维持；已排除元数据侧原因（方案 A 作用域、inode 表 write-behind、完全异步三臂 A/B 均无差别）⇒ 机制在数据路径 / 脏页节流 / 分配一侧。

---

生成命令：`python3 test/aster-ext4-baseline-test/make_results_doc.py`

