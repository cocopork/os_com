# rNVPCe range write transactions (2026-10-31)

## Scope

This experiment validates the implementation described by plan 019. The Native
rNVPCe write path plans one logical range, publishes one redirect entry per
logically and physically contiguous NV range, owns its slots as one range, and
locks/persists an existing NV entry once instead of rebuilding a per-page
`PersistentObjectRef`. Requests larger than 128 pages loop over bounded range
transactions; a 1 MiB aligned write therefore completes as two 128-page
transactions rather than a short write.

Final performance image source commit: `a1e424eb8` (`Finalize rNVPCe range
transaction compatibility`). Native data slots are owned only by their redirect
range; generation-zero compatibility publications retain the legacy per-page
object lifecycle without adding it back to the Native path. The branch includes
later selector/inode failure-protocol hardening that was correctness-tested but
not re-benchmarked; the throughput table therefore remains explicitly tied to
the `a1e424eb8` image. The final correctness hardening is commit `9d4b0dc21`
(`Harden rNVPCe range publication failures`).

Release image SHA-256:

```text
e04aef1176645f868309ce8958deca6ee995c8244753951b4cb6d8c279f85b2d  selected.iso
b4a0a59deea43e7017998c1f56fc40c6a528b522f387737e693fea4edc05da08  current.elf
```

The committed source and final release image keep latency profiling disabled.
The structural diagnostic below is retained from the earlier `fa7af3d8c`
diagnostic image and is labelled separately; it is not presented as timing from
the final release image.

## Configuration

- Device: authorized `/dev/nvme1n1p1`, reformatted no-journal EXT4 before each point.
- VM: 64 vCPUs, 8 GiB RAM, one virtio-blk queue, queue size 1024.
- Pool: 128 MiB total, 64 MiB metadata reserve, 64 MiB data limit, fast NVDIMM simulation.
- Workload: 2 GiB, 128 KiB sequential buffered overwrite, `psync`, jobs 1 and 16.
- Procedure: three alternating rounds; every point performs `sync`, unmount, and host `e2fsck -fn`.
- Final raw artifacts: `/home/zn/asterinas-build-env/rnvpce-range-write-20261031-a1e424eb8`; these are intentionally not tracked.

## Correctness

`cargo osdk check --ktests --package aster-kernel` passed. Exact KVM tests for
the 1 MiB request loop, range-owned slots, data-slot recovery, legacy redirect
publication/reclaim, random overwrite, FAIR normalization, selector crash
boundaries, stale-generation cleanup, short-copy allocation ownership, domain
allocation, the aggregate EXT4 research routes, and both rNVPC WAL recovery
cases passed. The WAL tests cover root-split recovery and WAL/soft-update
ownership in one shared pool.

Final failure-protocol tests also passed: an inode-publication fault durably
clears its selector, permits a same-generation retry, and after restart exposes
only the retry data; a range-owner validation failure returns every claimed
entry to `Dirty`; and a latched uncertain selector state rejects an existing-NV
overwrite with `EUCLEAN` before changing its bytes. Foreground write admission,
range mutation, migration claim, and reclaim all use the same fail-stop gate.

The final monolithic KVM run completed with 401 passed and 7 failed. The
remaining failures are order-dependent legacy tests around shared allocation
cache/slot allocator initial state and cumulative fence counters. The directly
affected range-publication, recovery, overwrite, migration, and EXT4 route
tests pass; this report does not claim the repository-wide suite is fully
green.

All six fio transfers wrote exactly 2 GiB, `sync` and unmount succeeded, and all
six `e2fsck -fn` runs exited 0. The range-owner ktest also checks one range-table
lock acquisition at publication, claim, and reclaim, writer exclusion during
migration, stale-`Arc` rejection after reuse, and recovery reconstruction.

## Release Performance

| jobs | fio samples (MiB/s) | fio median | durable samples (MiB/s) | durable median |
|---:|---|---:|---|---:|
| 1 | 3531.03, 3250.79, 2412.25 | 3250.79 | 1932.08, 1551.52, 1312.82 | 1551.52 |
| 16 | 1928.44, 1944.92, 2560.00 | 1944.92 | 1163.64, 1183.82, 1393.20 | 1183.82 |

All six points transferred exactly 2 GiB, and all six post-unmount checks
returned `e2fsck=0`. The historical comparison points were 520.19 MiB/s (jobs
1) and 288.25 MiB/s (jobs 16). They came from an earlier implementation and
remain development references rather than a same-image A/B run. The final
jobs=16 samples have substantial run-to-run spread, so they support removal of
the per-page transaction bottleneck but do not establish a stable scalability
curve.

## Earlier Structural Diagnostic (`fa7af3d8c`)

The table is the final cumulative snapshot from the jobs=1 diagnostic run.
`units` is pages for background stages and bytes for foreground stages.

| stage | calls | units | mean pages/call | total time |
|---|---:|---:|---:|---:|
| foreground range writes | 16,384 | 2,147,483,648 bytes | 32 pages/request | 602.62 ms |
| capacity waits | 209 | 6,688 pages | 32.0 | 89.23 ms |
| candidate select | 590 | 524,288 pages | 888.6 | 4.66 ms |
| flush/data submit | 571 | 524,288 pages | 918.2 | 576.35 / 571.48 ms |
| completion | 571 | 524,288 pages | 918.2 | 140.60 ms |
| device flush | 571 | 524,288 pages | 918.2 | 132.56 ms |
| extent publish/reclaim help | 555 | 524,288 pages | 944.7 | 123.10 / 123.17 ms |

In that diagnostic image, the legacy per-page completion stages `extent_member_validate`,
`extent_reverse_lookup`, `extent_leaf_read`, and `extent_leaf_validate` all
reported zero calls. This established that migration handles the
524,288 pages in hundreds of batches rather than repeating tree identity work
for every page. The final implementation goes further: Native publication,
recovery, and reclaim no longer install per-slot `ObjectRecord` values at all.
Only generation-zero compatibility APIs keep those records. Because profiling
was disabled for the final release measurement, the earlier stage timings must
not be compared numerically with the final throughput samples.

## Reproduction

Build a release image with `pipeline_profile::ENABLED=false`, place it as
`selected.iso` beside fresh `ext2.img` and `exfat.img`, then run:

```sh
ART=/path/to/artifacts TARGET=/dev/nvme1n1p1 \
  docs/experiments/scripts/rnvpce-range-write-20261031/run.sh --authorized-target
python3 docs/experiments/scripts/rnvpce-range-write-20261031/summarize.py \
  /path/to/artifacts
```

The runner is destructive and requires the explicit authorization marker in
the delegated harness. Raw logs, images, generated CSV, and plots stay outside
Git; only this report and the reproduction scripts are tracked.
