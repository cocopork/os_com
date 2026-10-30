# repair-image 与 page-redirection 的 4 KiB thread sweep

日期：2026-10-29。

## 结论

repair-image 的五个点均完整写入 2 GiB；当前 page-redirection 工作树只有 j1 完成，
j2/j4/j8/j16 均在写入 1.65--1.78 GiB 后由 fio 收到 `ENOSPC`。当前方案 j1
带宽为 22.62 MiB/s，仅为 repair-image 的 16.9%。因此当前方案同时存在首次写关键
路径性能回退和多线程内部容量耗尽，不能把四个失败点组成的曲线解释为有效扩展性。

## 条件

- `/dev/nvme1n1p1`，每点重新建立无 journal EXT4，测试后执行 `e2fsck -fn`；
- rNVPCe + rTIO，fast NVDIMM 模拟，128 MiB pool、64 MiB metadata reserve、
  64 MiB data limit；
- 64 vCPU、8 GiB、单 virtio queue depth 1024，rTIO CPU 隔离；
- fio buffered psync、`rw=write`、`bs=4k`、`iodepth=1`、总量 2 GiB，
  jobs 为 1/2/4/8/16；
- repair-image ISO SHA256：
  `a24a4158c64754fa547a04538351e62d2e3be4980aa22b4e92ef5237446845b8`；
- 当前方案 ISO SHA256：
  `1b4a735b4c0ad1094f90c5eeaf2da03247506b4c5504ba02b8fab053c8687bcf`。

repair-image 使用历史无插桩构建，对应 `RNVPCE-FOREGROUND-BATCH-2026-10-19.md`；
当前方案使用 baseline-token-once 优化后的默认构建。两者诊断探针均关闭。

## 完整结果

| jobs | repair fio MiB/s | repair drain MiB/s | repair mean us | repair P99 us | 当前结果 | 当前 fio MiB/s | 当前写入 MiB | 当前 mean us | 当前 P99 us |
|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|
| 1 | 133.50 | 97.06 | 28.94 | 79.36 | complete | 22.62 | 2048.00 | 172.34 | 183.30 |
| 2 | 118.90 | 84.11 | 65.08 | 138.24 | ENOSPC | 23.80 | 1654.57 | 261.53 | 460.80 |
| 4 | 137.96 | 93.26 | 112.01 | 211.97 | ENOSPC | 49.87 | 1781.77 | 247.51 | 1138.69 |
| 8 | 158.11 | 106.06 | 195.57 | 423.94 | ENOSPC | 56.87 | 1741.79 | 434.85 | 880.64 |
| 16 | 133.75 | 91.92 | 462.81 | 962.56 | ENOSPC | 81.26 | 1747.09 | 644.94 | 1875.97 |

失败点中的 fio MiB/s 只描述错误发生前的部分写入速率；由于未完成 2 GiB，报告不计算
drain 带宽，也不将其与 repair-image 的完整结果作等价性能比较。

## 正确性与现象

- repair-image 5/5 点：fio status 0、完整 2 GiB、sync/umount 成功、e2fsck 通过；
- 当前 j1：fio status 0、完整 2 GiB、sync/umount 成功、e2fsck 通过；
- 当前 j2/j4/j8/j16：fio errno 28，sync/umount 成功，部分结果均通过 e2fsck；
- 失败时底层 256 GiB 文件系统只使用约 1.7--1.8M/67.1M blocks，因此不是物理设备
  空间耗尽；`ENOSPC` 来自当前 rNVPCe 首次写/SSD baseline 路径的内部资源或账本；
- j1 没有触发容量错误，但 mean latency 是 repair-image 的 5.96 倍，说明容量错误之外
  还存在独立的逐 4 KiB 首次写开销。

## 证据

- 原始日志、逐点 e2fsck 和 CSV：
  `/home/zn/asterinas-build-env/repair-vs-page-redirection-4k-20261029/`；
- 可复现实验脚本：
  `docs/experiments/scripts/repair-vs-page-redirection-4k-20261029/run-sweep.sh`。

原始镜像和运行日志不纳入 Git。
