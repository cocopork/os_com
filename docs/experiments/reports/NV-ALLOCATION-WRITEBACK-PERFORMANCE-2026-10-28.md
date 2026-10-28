# NVM Allocation Metadata Writeback Performance (2026-10-28)

This experiment measures commit `de600378f` on branch
`recovery/rnvpce-page-redirection` after removing allocation
`mutation_epoch`, retaining persistent `generation`, and serializing each block
group's data migration and allocation-metadata writeback ownership.

## Configuration

- Device: authorized `/dev/nvme1n1p1`; reformatted before every point as 4 KiB
  EXT4 and checked with `e2fsck -fn` after unmount.
- QEMU/KVM: 64 vCPUs, 8 GiB, one virtio-blk queue with depth 1024 and the
  historical CPU-isolation configuration.
- rNVPCe: Native mode, pool 128 MiB, metadata reserve 64 MiB, data high
  watermark 64 MiB, low watermark 32 MiB, fast persist simulation, no JBD2.
- fio: buffered `psync`, `direct=0`, `fsync=0`, depth 1 per job, 2 GiB total.
  Random write uses 4 KiB and pre-fills its files; sequential write uses 128 KiB.
- Each point is one sample. The unprofiled release image is the throughput
  result. A separate image with `pipeline_profile::ENABLED=true` supplies stage
  timing and is not used as the release throughput result.

All eight runs (four release and four diagnostic) wrote the planned 2 GiB with
fio error 0, synced and unmounted successfully, and passed the final filesystem
check.

## Release Throughput

`With drain` covers fio start through the final `sync`, but excludes unmount.

| Workload | Jobs | fio MiB/s | With drain MiB/s | fio time | Post-fio drain/sync |
| --- | ---: | ---: | ---: | ---: | ---: |
| 4 KiB random write | 1 | 165.21 | 155.27 | 12.396 s | 0.794 s |
| 4 KiB random write | 16 | 125.01 | 118.04 | 16.383 s | 0.967 s |
| 128 KiB sequential write | 1 | 212.36 | 141.93 | 9.644 s | 4.786 s |
| 128 KiB sequential write | 16 | 86.98 | 73.56 | 23.545 s | 4.295 s |

Adding jobs reduces throughput for both workloads. The 128 KiB/16-job point is
the largest regression: it reaches only 41.0% of the single-job fio bandwidth.

## Pipeline Breakdown

The following values come from the diagnostic image. Totals are accumulated
CPU/wait time and overlap across foreground and background threads; they must
not be summed into wall-clock time. `Average` is total divided by calls.

| Stage | 4K/j1 total / average | 4K/j16 total / average | 128K/j1 total / average | 128K/j16 total / average |
| --- | ---: | ---: | ---: | ---: |
| Foreground write | 11.862 s / 22.6 us | 213.667 s / 407.5 us | 10.673 s / 651 us | 332.572 s / 20.30 ms |
| Foreground persist | 4.882 s / 9.31 us | 46.782 s / 89.2 us | 2.168 s / 132 us | 12.179 s / 743 us |
| Capacity wait | 3.767 s / 7.85 ms | 83.516 s / 6.67 ms | 6.597 s / 13.35 ms | 199.042 s / 2.94 ms |
| Flush round | 8.658 s / 16.91 ms | 9.817 s / 19.17 ms | 10.281 s / 20.08 ms | 9.994 s / 19.52 ms |
| Candidate select | 7.677 s / 13.73 ms | 10.998 s / 14.28 ms | 9.708 s / 18.21 ms | 21.709 s / 14.55 ms |
| Target prepare | 0.422 s / 0.822 ms | 0.498 s / 0.969 ms | 0.349 s / 0.679 ms | 0.357 s / 0.695 ms |
| Data BIO submit | 1.025 s / 2.00 ms | 0.896 s / 1.75 ms | 0.352 s / 0.687 ms | 0.532 s / 1.04 ms |
| Async data-ready wait | 5.212 s / 10.18 ms | 4.829 s / 9.43 ms | 0.678 s / 1.32 ms | 0.674 s / 1.32 ms |
| Completion total | 2.055 s / 4.01 ms | 4.793 s / 9.36 ms | 3.399 s / 6.64 ms | 17.650 s / 34.47 ms |
| SSD baseline publish | 1.899 s / 3.71 ms | 4.592 s / 0.640 ms | 3.284 s / 6.41 ms | 17.537 s / 3.59 ms |
| Device flush | 0.067 s / 0.130 ms | 0.067 s / 0.131 ms | 0.085 s / 0.166 ms | 0.072 s / 0.141 ms |

Every workload migrated exactly 524,288 pages in 512 data batches. The
candidate selector performed additional attempts when a same-group owner was
still active: 559, 770, 533, and 1,492 calls respectively.

## Findings

1. Target allocation and NVM allocation-metadata publication are not the direct
   hot path in this run. `target_prepare` is below 0.50 seconds accumulated for
   every 2 GiB point and below 1 ms per call. This supports the intended rule
   that rTIO target preparation performs no synchronous SSD metadata I/O.
2. The 128 KiB/16-job bottleneck is dominated by backpressure and SSD baseline
   publication. Foreground capacity wait accumulates 199.0 seconds. Completion
   accumulates 17.65 seconds, of which baseline publication accounts for 17.54
   seconds (99.4%). Device flush itself is negligible.
3. Candidate selection remains expensive at roughly 14-18 ms per attempt and
   is most of each 17-20 ms flush round. The same-group ownership rule increases
   attempts under the 16-job sequential workload, but target preparation stays
   cheap after a group is acquired.
4. Sequential write has a 4.3-4.8 second post-fio tail, versus less than one
   second for random overwrite. The current profiler does not split this tail:
   it combines remaining rTIO drain, native extent export, blocking allocation
   metadata writeback, and the final device sync. It is therefore not valid to
   attribute the whole tail to allocation-metadata writeback.

The next diagnostic should add separate timing around
`writeback_rnvpce_allocation_group`: stable-image capture, block bitmap/GDT
write, superblock write, device flush, and NVM-to-SSD authority handoff. The
main runtime optimization target remains 128 KiB/16-job capacity turnover and
batched SSD baseline publication rather than `mutation_epoch` removal.

## Evidence

- Release logs, manifests, status, fio summary, and filesystem checks:
  `/home/zn/asterinas-build-env/nv-allocation-writeback-20261028/final/`
- Diagnostic logs and extracted `pipeline-metrics.csv` / `pipeline-stages.csv`:
  `/home/zn/asterinas-build-env/nv-allocation-writeback-20261028/diagnostic/`

Raw logs and disk images remain outside Git. This report is the tracked summary.
