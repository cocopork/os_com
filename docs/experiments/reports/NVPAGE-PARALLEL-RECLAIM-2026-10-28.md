# NVPage Parallel Reclaim (2026-10-28)

## Implementation

This experiment implements the runtime scheduling phase of plan 017 on the
current per-page descriptor redirection architecture:

- completion performs the batch device flush and publishes reclaim-ready tickets;
- with multiple capacity waiters, foreground threads claim tickets before sleeping and run the
  same bounded helper;
- with zero or one capacity waiter, the background worker consumes tickets so a single writer
  does not execute publication and cleanup inline;
- helper completion clears exact redirects, batch-frees NVPages, and releases block-group ownership;
- `io_in_flight` and `reclaim_pending` are accounted and drained independently;
- data pressure uses a 64 MiB high watermark and 32 MiB low watermark;
- stale allocation-metadata scan snapshots are classified as `ESTALE`, not corruption.

The later per-page descriptor design supersedes plan 017's mixed SSD/NV extent target.
This phase therefore does not reintroduce leaf target switching. The proposed per-CPU
free-run cache and sharded global free pool are not implemented yet.

## Configuration

- Authorized `/dev/nvme1n1p1`, freshly formatted as 4 KiB EXT4 for each point.
- QEMU/KVM: 64 vCPUs, 8 GiB, one virtio-blk queue, depth 1024, historical CPU isolation.
- rNVPCe: 128 MiB pool, 64 MiB metadata reserve, 64/32 MiB data high/low watermarks.
- fio 3.39: buffered `psync`, depth 1 per job, 2 GiB total.
- Results are one sample per point; the diagnostic image is separate from throughput results.

## Throughput

| Workload | Jobs | Before MiB/s | Parallel reclaim MiB/s | Change |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB random write | 1 | 165.21 | 112.04 | -32.2% |
| 4 KiB random write | 16 | 125.01 | 107.85 | -13.7% |
| 128 KiB sequential write | 1 | 212.36 | 160.56 | -24.4% |
| 128 KiB sequential write | 16 | 86.98 | 91.93 | +5.7% |

All completed final points wrote 2 GiB with fio error 0, synced and unmounted cleanly.
The final device image passed `e2fsck -fn`.

## 128 KiB / 16-job Breakdown

Totals are accumulated across threads and overlap; they cannot be summed into wall time.

| Stage | Calls | Total | Average |
| --- | ---: | ---: | ---: |
| Capacity wait | 10,891 | 320.519 s | 29.43 ms |
| Flush round | 512 | 20.795 s | 40.62 ms |
| Candidate select | 698 | 23.826 s | 34.13 ms |
| Completion total | 512 | 1.295 s | 2.53 ms |
| Device flush | 512 | 0.089 s | 0.174 ms |
| SSD baseline publish and reclaim | 5,287 | 103.354 s | 19.55 ms |
| Reclaim-ready queue wait | 5,287 | 0.503 s | 95.06 us |
| Reclaim helper | 5,287 | 103.355 s | 19.55 ms |

Completion and ready-queue wakeup are no longer the limiting stages. `nv_extent_publish`
accounts for effectively the entire helper time: the expensive work is SSD baseline publication
and descriptor cleanup. Parallel execution improves only
the 16-job sequential point; accumulated helper contention and capacity wait remain large, and
all other points regress. The performance acceptance criteria are therefore not met. The next
optimization should split and batch the helper body, then add the deferred per-CPU/sharded
free-run allocator; merely adding more helper wakeups is not sufficient.

## Evidence

- Release logs and filesystem checks:
  `/home/zn/asterinas-build-env/nvpage-parallel-reclaim-20261028/final/`
- Diagnostic image and raw pipeline output:
  `/home/zn/asterinas-build-env/nvpage-parallel-reclaim-20261028/diagnostic/`

Raw logs and disk images remain outside Git. This report is the tracked summary.

## Latest-Code Rerun (2026-10-29)

Commit `459f37f95` was rebuilt with pipeline profiling enabled and rerun with the same
128 KiB sequential-write, 16-job, 2 GiB configuration. The frozen diagnostic ISO is
`1bde9606775a8119a3352c75cc5f782dac0b8cf09e9eafe8fa7c1c5de9c559a2`.

fio completed 2 GiB with error 0 in 17.075 s: 125,767,709 B/s (119.94 MiB/s),
959.53 IOPS, and 15.677 ms mean completion latency. Sync after fio took another
6.61 s, and unmount succeeded. A read-only `e2fsck -fn` completed all five passes
without filesystem errors; its only notes were that several extent trees could be narrower.

Totals below overlap across foreground threads and workers and cannot be summed into
wall time. The previous column is the 2026-10-28 diagnostic run above, not an
uninstrumented throughput result.

| Stage | Calls | Total | Average | Previous average | Change |
| --- | ---: | ---: | ---: | ---: | ---: |
| Foreground write | 16,384 | 256.796 s | 15.674 ms | 23.005 ms | -31.9% |
| Foreground persist | 16,384 | 15.457 s | 0.943 ms | 1.083 ms | -12.9% |
| Capacity wait | 12,039 | 201.127 s | 16.706 ms | 29.430 ms | -43.2% |
| Flush round | 512 | 13.776 s | 26.906 ms | 40.616 ms | -33.8% |
| Candidate select | 637 | 16.004 s | 25.124 ms | 34.134 ms | -26.4% |
| Target prepare | 514 | 0.419 s | 0.815 ms | 0.873 ms | -6.7% |
| Data submit | 512 | 0.471 s | 0.919 ms | 1.028 ms | -10.6% |
| Async I/O ready wait | 512 | 0.731 s | 1.428 ms | 1.623 ms | -12.0% |
| Completion queue wait | 512 | 0.014 s | 0.028 ms | 0.023 ms | +20.9% |
| Completion total | 512 | 0.203 s | 0.397 ms | 2.530 ms | -84.3% |
| Device flush | 512 | 0.088 s | 0.172 ms | 0.174 ms | -0.9% |
| SSD baseline publish and reclaim | 5,895 | 117.548 s | 19.940 ms | 19.549 ms | +2.0% |
| Reclaim-ready queue wait | 5,895 | 1.044 s | 0.177 ms | 0.095 ms | +86.3% |
| Reclaim helper | 5,895 | 117.549 s | 19.940 ms | 19.549 ms | +2.0% |

The rerun does not change the bottleneck diagnosis. Device flush and completion are
small. The helper still spends effectively all of its time in SSD baseline publication,
migration-credit handling, exact redirect clearing, and NVPage return. Its per-call cost
is stable within 2% across the two runs. Foreground throughput improved because capacity
wait and candidate-selection time happened to be lower in this sample; because both runs
use the same commit, that difference is run-to-run variability rather than evidence of a
new code optimization.

Rerun artifacts are under
`/home/zn/asterinas-build-env/nvpage-parallel-reclaim-20261029/diagnostic/`.
