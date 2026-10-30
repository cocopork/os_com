# Foreground SSD Baseline Performance, 2026-10-29

## Configuration

- Branch: `recovery/rnvpce-page-redirection`
- Device: `/dev/nvme1n1p1`, reformatted before every point
- Guest: 64 vCPU, 8 GiB RAM; fio CPUs 0-31, Flush CPU 60, Virtio CPU 62
- Virtio-blk: one queue, depth 1024
- rNVPCe: 128 MiB pool, 64 MiB metadata reserve, 64 MiB data limit,
  32 MiB effective low water mark, fast persistence simulation
- fio: buffered `psync`, 2 GiB total, `iodepth=1`, one sample per point
- Workloads: 4 KiB random write and 128 KiB sequential write, 1 and 16 jobs

The drain bandwidth divides 2 GiB by the interval from `BUFFER_FIO_START` to
the end of the final `sync`. It therefore includes draining rNVPCe and SSD
state, unlike fio's foreground-only bandwidth.

## Results

| Workload | Jobs | fio bandwidth | Drain bandwidth | IOPS | Mean clat | P99 clat |
|---|---:|---:|---:|---:|---:|---:|
| 4 KiB randwrite | 1 | 156.43 MiB/s | 148.08 MiB/s | 40.05k | 24.59 us | 35.58 us |
| 4 KiB randwrite | 16 | 137.14 MiB/s | 130.53 MiB/s | 35.11k | 446.38 us | 9.24 ms |
| 128 KiB write | 1 | 110.88 MiB/s | 86.23 MiB/s | 887 | 1.13 ms | 20.84 ms |
| 128 KiB write | 16 | 113.60 MiB/s | 92.75 MiB/s | 909 | 16.86 ms | 127.40 ms |

All four runs reported `fio error=0`, wrote exactly 2,147,483,648 bytes,
unmounted successfully, and passed offline `e2fsck -fn`.

## Failure Found During Measurement

The first 128 KiB sequential run failed near the end of 2 GiB although the
SSD had about 249 GiB free. Two independent causes were found:

1. Foreground allocation selected the block group with the most free space for
   every syscall. Sequential requests therefore rotated across groups and did
   not form physically contiguous SSD extents. Allocation now uses the block
   after the preceding logical page as its first goal.
2. Every foreground write retained a 4 KiB `AllocationRecord`. A 64 MiB
   metadata reserve therefore exhausted after about 16,384 writes. The record
   is now reclaimed after the inode SSD baseline is published; the
   NVM-authoritative bitmap and baseline remain the recovery proof.

The `nv_route_rnvpce_first_write_publishes_ssd_baseline` regression scenario
checks both cross-syscall physical continuity and that foreground writes do not
increase the persistent allocation-record count.

## Interpretation

- The metadata leak is fixed: both 128 KiB points now complete all 16,384
  writes instead of failing with `ENOSPC`.
- More foreground threads do not improve 128 KiB throughput. Aggregate
  bandwidth stays near 111-114 MiB/s while mean latency increases by about
  15 times, which points to a shared software path rather than insufficient
  fio concurrency.
- Against the pre-cleanup sample from the same session (168.66 MiB/s at one
  job and 130.67 MiB/s at 16 jobs), the final 4 KiB result is 7.2% lower at one
  job but 5.0% higher at 16 jobs. Immediate ownership-record reclamation does
  add foreground metadata work, but repeated samples and stage profiling are
  required before attributing the single-thread difference to that cleanup.

Raw console and e2fsck logs are intentionally untracked under
`/home/zn/asterinas-build-env/foreground-baseline-20261029/final/`.
