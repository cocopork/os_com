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
