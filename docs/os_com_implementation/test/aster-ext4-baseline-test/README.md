# Asterinas EXT4 FIO Baseline

## Purpose

Measure the Asterinas EXT4 buffered-write path on dedicated SATA or NVMe test partitions. This is a harness and diagnostic source, not an accepted throughput claim; see [`../../EXPERIMENTS.md`](../../EXPERIMENTS.md) for acceptance status.

## Matrix

| Dimension | Values |
| --- | --- |
| Device | `sata` (`/dev/sda4`), `nvme` (`/dev/nvme3n1p2`) |
| Profile | `ext4` by default; `ext2like` only when explicitly selected |
| Workload | `randwrite-4k`, `seqwrite-4k`, and supported mixed/read diagnostics |
| Mode | `orderless`, `fsync` |
| Jobs | `1,4,8,12,16,20,24,28,32` |

The `ext2like` profile retains extents and has known no-journal metadata writeback limitations, so it is not a Linux ext2 equivalent. FIO uses one file per job, `fallocate=none`, 4 KiB alignment, and a fresh filesystem per point.

## Prerequisites

Follow the shared [test guide](../README.md). KVM, Docker, a built `os_com` tree, and the selected unmounted test partition are required. The normal VM configuration is `smp=64`, `mem=4G`; vCPU count must exceed the largest job count. Use `--skip-build` only for a frozen kernel tree and guest script: the harness compares its build stamp before reusing an image.

## Command

```bash
bash run-baseline.sh --dry-run
bash run-baseline.sh --confirm-destroy --devices sata --workloads randwrite-4k --modes orderless --njobs 1,4,8 --fs ext4
```

Use `--timeout 300` to set the per-point wall-clock limit. Keep the default build for a changed source tree; use `--skip-build` only after the stamp check passes.

## Output

`results/<timestamp>_aster-ext4-baseline*/summary.csv` contains `busy_bw_mib_s`, diagnostic `drain_bw_mib_s`, validity, and a failure note. The matching result directory contains parser input and QEMU/serial logs. Guest records include `FIO_RESULT`, `PARAM`, `PHASE`, `END_FIO_RESULT`, and `DRAIN`; the launcher requires a new `END_FIO_RESULT` for each point.

## Acceptance checks

Keep only parser rows with `valid=1`, at least 10 MiB written, a completion marker, and the matching parameter snapshot. Use `busy_bw_mib_s` for formal plots and tables. Preserve repeated comparable points, then package only a final accepted summary, plot source, and final figure under `evidence/`; do not package raw logs. Drain timing is diagnostic and never a formal metric.

## Known limitations

The harness formats each selected partition. It measures an Asterinas EXT4 configuration rather than Linux ext2, and instrumentation changes performance. The one-queue virtio path, VM memory pressure, timeouts, and host contention can invalidate a point. Historical plots and summaries are diagnostic until their complete provenance is accepted.
