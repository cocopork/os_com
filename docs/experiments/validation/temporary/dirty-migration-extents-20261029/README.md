# Dirty Migration Extent Index and Batched Reclaim

Date: 2026-10-29

Branch: `recovery/rnvpce-page-redirection`

## Result

The rTIO candidate path now uses bounded dirty migration extents.  Every extent
contains at most 128 logically and physically contiguous NVPages.  Publication,
replacement, and clearing update a partition-local ready-page index; candidate
claim materializes only the selected extent and no longer rebuilds or globally
sorts every dirty page.

Completion/reclaim now collects several ready extent work items, groups their new
SSD baselines by inode, publishes each inode's baseline once, then clears exact
descriptors and frees NVPages in batches.  The 128-page limit remains the
structural migration-lock boundary; it is not enlarged by reclaim aggregation.

## Implementation

- Added checked `DirtyMigrationExtent` construction with a hard 128-page cap.
- Added partition-local ready queues, exact-handle leases, logical overlap
  checks, and physical NV-slot range leases.
- Replaced eager partition-wide extent reconstruction with lazy construction
  from an indexed ready key.  Recovery performs the only full rebuild.
- Reserved EXT4 allocation groups before target mutation and skipped target
  preparation entirely for empty migration rounds.
- Changed descriptor removal and NVPage freeing to batch operations, avoiding
  repeated partition, global state, and allocation-lock acquisition.
- Aggregated multiple completed migration extents into one SSD-baseline update
  per inode and reclaim helper invocation.
- Kept foreground capacity waiting based on the requested reservation.  The
  low watermark controls background pressure; making it a foreground exit
  condition can deadlock behind private, not-yet-published reservations.

## Test Configuration

| Item | Value |
|---|---|
| Device | `/dev/nvme1n1p1`, 256 GiB, reformatted EXT4 per run |
| VM | 64 vCPU, 8 GiB RAM, KVM |
| I/O | fio 3.39, `psync`, buffered, 2 GiB total |
| rNVPCe | Native mode, rTIO enabled |
| NVDIMM pool | 128 MiB total, 64 MiB metadata reserve |
| Dirty data limit | 64 MiB, low watermark 32 MiB |
| virtio-blk | one queue, depth 1024, request merging disabled |
| CPU placement | fio CPUs 0-31; rTIO/virtio isolation enabled |

## End-to-End Performance

| Workload | Threads | Bandwidth | fio runtime |
|---|---:|---:|---:|
| 4 KiB random write | 1 | 151.58 MiB/s | 13.51 s |
| 4 KiB random write | 16 | 146.31 MiB/s | 14.00 s |
| 128 KiB sequential write | 1 | 62.26 MiB/s | 32.89 s |
| 128 KiB sequential write | 16 | 161.76 MiB/s | 12.66 s |

For the 128 KiB single-thread case, the successive implementation checkpoints
were 25.32 MiB/s (initial extent path), 26.86 MiB/s (batch descriptor removal),
27.09 MiB/s (lazy index), 27.67 MiB/s (batch free), and 62.26 MiB/s after
multi-extent SSD-baseline publication.  The final step is a 2.25x improvement
over the last pre-aggregation result.

## Diagnostic Decomposition

The diagnostic build instruments hot paths and therefore lowers the same
128 KiB single-thread workload to 53.31 MiB/s.  Times below are diagnostic
overhead-inclusive and should be used to compare stages and call granularity,
not as release throughput.

| Stage | Calls | Pages/bytes represented | Total | Mean/call |
|---|---:|---:|---:|---:|
| Foreground write | 16,384 | 2 GiB | 38.371 s | 2.342 ms |
| Foreground persist | 16,384 | 2 GiB | 12.623 s | 770.4 us |
| Capacity wait | 257 | 8,224 pages | 0.894 s | 3.479 ms |
| Candidate select | 344,139 | 524,288 pages | 1.733 s | 5.0 us |
| Target prepare | 11,238 | 524,288 pages | 5.218 s | 464.3 us |
| Data submit | 11,238 | 524,288 pages | 1.118 s | 99.5 us |
| Completion total | 11,238 | 524,288 pages | 2.157 s | 191.9 us |
| Device flush | 11,238 | 524,288 pages | 2.042 s | 181.7 us |
| Aggregated baseline prepare | 515 | 524,288 pages | 31.873 s | 61.89 ms |
| Reclaim helper / extent publish | 515 | 524,288 pages | 37.362 s | 72.55 ms |

The reclaim path averages 1,018 pages per helper call.  Before aggregation it
performed about 24,553 small extent-publish flows; the new path performs 515.
Candidate selection is no longer dominated by a full scan, but 344,139 queue
attempts for 524,288 pages remain excessive.  Reducing empty/stale ready-key
visits is the next clear selector optimization.  Baseline mutation also remains
the largest serialized stage even after its call count was reduced.

## Correctness Verification

- `cargo osdk check --ktests --package aster-kernel`: passed.
- `cargo osdk test ext4_research_routes`: passed all 21 scenarios.
- `cargo osdk test rnvpce_dirty_extent_index`: passed.
- Full kernel ktest suite under KVM: `380 passed; 0 failed; 0 filtered out`.
- All four fio runs completed with `fio_status=0` and unmounted successfully.
- `e2fsck -fn /dev/nvme1n1p1`: passed all five passes; only the informational
  "extent tree could be narrower" optimization suggestion was reported.

Raw logs and generated images are intentionally untracked under:

`/home/zn/asterinas-build-env/dirty-migration-extents-20261029/`
