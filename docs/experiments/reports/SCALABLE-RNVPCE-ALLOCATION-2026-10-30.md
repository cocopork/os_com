# Scalable rNVPCe SSD Allocation

Date: 2026-10-30

Branch: `recovery/rnvpce-page-redirection`

## Change Under Test

- per-CPU block-group selection with atomic free-space summaries;
- group-local generation-tagged migration leases;
- existing group-local `FreeSpaceIndex` as the bitmap search authority;
- stable NVM allocation bitmap chunks with aligned 64-bit word persistence;
- immutable group writeback snapshots with generation-checked, redirty-safe
  authority handoff;
- rNVPCe capacity-ledger updates on the allocation hot path, with the derived
  superblock count reconciled by allocation-metadata writeback;
- explicit pre-mutation/uncertain persistence outcomes; uncertain target
  allocations remain allocated and charged as conservative leaks.

Instrumentation in `allocator::metrics` remains disabled for throughput runs.

## Configuration

- 64-vCPU QEMU/KVM guest, rTIO CPU isolation enabled;
- 64 MiB rNVPC data limit, 128 MiB total simulated NVDIMM;
- `/dev/nvme1n1p1`, reformatted EXT4 for every point;
- 2 GiB buffered first-write workload, `psync`, queue depth 1 per fio job;
- virtio-blk queue depth 1024, one request-forwarding queue;
- `sync`, unmount, and `e2fsck -fn` required after every point.

## Results

Raw fio/QEMU/e2fsck logs are intentionally not tracked by Git. They are stored
under `/home/zn/asterinas-build-env/scalable-rnvpce-allocation-20261030-final3`.
The tested ISO SHA-256 is
`82d8c9c553cea8b775a990dace6a6a23d045a980497af2513df053e65fa65e0e`.

| Block size | Jobs | fio MiB/s | Drained MiB/s | Mean latency | P99 latency | e2fsck |
|---|---:|---:|---:|---:|---:|---|
| 4 KiB | 1 | 65.39 | 54.74 | 59.39 us | 140.29 us | pass |
| 4 KiB | 2 | 105.55 | 79.41 | 71.03 us | 136.19 us | pass |
| 4 KiB | 4 | 164.70 | 113.02 | 79.42 us | 173.06 us | pass |
| 4 KiB | 8 | 189.37 | 117.70 | 161.13 us | 230.40 us | pass |
| 4 KiB | 16 | 164.00 | 87.15 | 368.94 us | 765.95 us | pass |
| 128 KiB | 1 | 267.64 | 137.45 | 465.81 us | 1269.76 us | pass |
| 128 KiB | 16 | 341.50 | 169.54 | 5780.44 us | 9109.50 us | pass |

`Drained MiB/s` covers fio start through the completion of the final `sync`, so
it includes rTIO migration and allocation-metadata writeback rather than only
the buffered foreground interval.

## Comparison

The pre-change page-redirection run recorded 22.62 MiB/s fio and 21.28 MiB/s
drained at 4 KiB/jobs=1; jobs 2-16 terminated with `ENOSPC`. The new single-job
result is 2.89x higher in the fio interval and 2.57x higher including drain,
and every multi-job point completed the full 2 GiB.

Against the repair-image reference, the new allocator is still slower for a
single 4 KiB writer (65.39 versus 133.50 MiB/s), but scales past it at jobs 4,
8, and 16. At jobs 8 the new result is 189.37 MiB/s fio and 117.70 MiB/s
drained, versus 158.11 and 106.06 MiB/s. Jobs 16 regresses from the jobs-8 peak
and its drained rate (87.15 MiB/s) is below repair-image (91.92 MiB/s), so
high-thread contention or migration drain remains a separate bottleneck.

Relative to the first implementation sweep, the final 4 KiB/jobs=16 result is
effectively unchanged (164.00 versus 165.75 MiB/s), while jobs=2 improves from
97.02 to 105.55 MiB/s. Removing per-allocation `super_block.write()` therefore
closes the architectural serialization point, but this single sweep does not
show a uniform throughput gain. Multiple repetitions are required before
attributing per-point changes to the allocator rather than run-to-run pipeline
variance.

## Allocator Diagnostics

The counter-only diagnostic used a separate `info`-level image with allocator
metrics enabled. Its logging volume intentionally disqualifies it as a
throughput measurement; the table below reports only allocator event counts.
The raw logs and checked images are stored under
`/home/zn/asterinas-build-env/scalable-rnvpce-allocation-20261030-diagnostic`.

| Metric | 4 KiB/jobs=1 | 4 KiB/jobs=16 | Interpretation |
|---|---:|---:|---|
| Allocation/group visits | 37,043 / 37,043 | 44,515 / 44,515 | exactly 1 group per allocation |
| Home candidates | 37,035 | 44,447 | normal CPU-local selection |
| Steal candidates | 0 | 0 | no global fallback needed |
| Group-busy retries | 6 (0.016%) | 57 (0.128%) | rare lease conflict |
| Summary false positives | 0 | 0 | atomic summary agreed with the locked index |
| Bitmap persist calls | 37,029 | 44,390 | range persistence invocations |
| Hot 64-bit bitmap updates | 37,022 | 44,317 | one word per ordinary allocation |
| Full 512-word chunk writes | 7 | 73 | group metadata admission/writeback only |

`bitmap_words - bitmap_persist_calls` is exactly `511` times the number of
full-chunk operations in both runs. Therefore every remaining hot-path call
persisted one aligned 64-bit word; it did not rewrite or validate a 4 KiB
allocation-bitmap page.

## Correctness

Focused OSDK tests cover selector fallback, group leases, stable bitmap word
updates, cross-chunk partial-persist recovery, failed free persistence,
multi-range rollback after a later `ENOSPC`, count reconstruction, missing
allocation-bit rejection during recovery, pager redirty behavior, and stale
pager-completion isolation. The final section records the full
`rnvpce_` and `ext4_research_routes` runs together with the real-device checks.

All seven real-device cases reported `fio_status=0` and `umount_status=0`.
Every subsequent `e2fsck -fn` completed passes 1-5 and reported no filesystem
repair requirement.

The two diagnostic cases also completed with `fio_status=0`,
`umount_status=0`, and clean `e2fsck -fn` passes.
