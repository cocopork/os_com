# Scalable NVPage Pool

Date: 2026-10-31

Branch: `recovery/rnvpce-page-redirection`

Tested revision: `0bd7862905fde75bf9b64fb95377e76684edd9ab`

## Result

The pool-wide physical free-list and capacity lock were replaced by fixed
allocation domains, per-domain free-range indexes, and atomic class-capacity
accounting. The rNVPCe redirect path also stopped duplicating publication,
migration, transition, and reclaim updates in the legacy `PageIndex`.

Three alternating real-device rounds produced 128 KiB sequential overwrite
medians of 520.19/288.25 MiB/s at jobs 1/16. Against the two available
same-semantics pre-change overwrite rounds (medians 438.38/226.21 MiB/s), this
is +18.7%/+27.4%. The changes are therefore effective for this workload,
although the experiment does not isolate how much of the gain comes from the
domain allocator versus removal of the duplicate legacy index.

## Implementation

- Physical data and metadata slots share one `NvSlotAllocator`.
- A 128 MiB pool on this 64-vCPU configuration is divided into fixed domains
  with a minimum target size of 1024 pages. A CPU starts at its home domain and
  falls back to other domains without holding two domain locks.
- Every domain uses a bitmap plus prefix/suffix/longest-free summaries. The
  normal path does not sort or scan the complete pool.
- Data, metadata, and WAL capacity counters are atomic. WAL consumes configured
  capacity but no physical slot.
- Recovery marks live physical slots in the same allocator instead of rebuilding
  a second global free vector.
- rNVPCe uses explicit redirect-page transitions and migration-target updates.
  Redirect publication/transition/clear/reclaim no longer mutates
  `PoolState::index`; the generic rNVPC transition still does.

Commits:

- `383e73969 Add partitioned NVPage slot allocator`
- `5c68c2067 Partition NVPage allocation state`
- `0bd786290 Isolate rNVPCe redirects from legacy index`

## Correctness

The following focused OSDK tests passed:

- domain allocator contiguous reuse, fallback, fragmentation, and randomized
  ownership model;
- parallel multi-page allocation uniqueness and capacity-class independence;
- WAL capacity without physical-slot consumption;
- high/low-water behavior, contiguous range continuation, failed reclaim
  rollback, and parallel reclaim accounting;
- redirect control-lock isolation while transition, publication,
  migration-target assignment, and reclaim execute;
- redirect selector crash boundaries, concurrent logical versions, old
  descriptor clearing, 128 KiB single-range publication, overwrite reuse,
  data-slot recovery, migration-target recovery, and batch reclaim durability.

`cargo osdk check --ktests --package aster-kernel` also completed successfully.
The source tree still emits pre-existing unused/dead-code warnings.

## Configuration

- Device: authorized `/dev/nvme1n1p1`, recreated as 4 KiB no-journal EXT4 for
  every point.
- Guest: QEMU/KVM, 64 vCPU, 8 GiB RAM, existing rTIO CPU isolation.
- NVDIMM simulation: 128 MiB total, 64 MiB metadata reserve, 64 MiB data limit,
  fast persistence mode.
- virtio-blk: one queue, depth 1024, host `cache=none`, `aio=threads`.
- fio: buffered `psync`, 128 KiB sequential overwrite, `iodepth=1`, 2 GiB total,
  jobs 1 and 16. Files were first populated with native direct I/O.
- Order: j1/j16, j16/j1, j1/j16.
- Every point required complete fio bytes, `sync`, successful unmount, and
  `e2fsck -fn` passes 1-5.

Release ISO SHA-256:
`65b25b3c218cfa235008f84329de5a5d115e67734e960203173ae8cf018d79b1`.

## Performance

| Round | Jobs | fio MiB/s | Including final sync MiB/s | fio s | sync s |
|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 520.19 | 439.48 | 4.45 | 0.21 |
| 2 | 1 | 518.48 | 436.67 | 4.45 | 0.24 |
| 3 | 1 | 604.66 | 522.45 | 3.69 | 0.23 |
| 1 | 16 | 288.25 | 259.57 | 7.47 | 0.42 |
| 2 | 16 | 292.24 | 258.59 | 7.53 | 0.39 |
| 3 | 16 | 272.99 | 247.04 | 7.92 | 0.37 |

| Jobs | Same-semantics pre-change median MiB/s | New median MiB/s | Change | New durable median MiB/s |
|---:|---:|---:|---:|---:|
| 1 | 438.38 | 520.19 | +18.7% | 439.48 |
| 16 | 226.21 | 288.25 | +27.4% | 258.59 |

The implementation plan originally listed 434.91/244.86 MiB/s as the two
pre-change references. Audit of the source CSV shows that 434.91 is a
first-write j1 sample, while 244.86 is an overwrite j16 sample. Comparing the
new medians to those mixed-semantics numbers gives +19.6%/+17.7%, but that is
only a directional plan comparison and not the same-semantics result above.
The matched historical overwrite baseline has only two rounds, so the exact
percentage should still be treated as preliminary rather than a final paper
result.

The release image intentionally disabled latency instrumentation, so these runs
do not provide direct allocation-domain lock wait/hold measurements. They prove
end-to-end improvement and correctness, not that per-domain contention is zero.
No CPU-local free-range cache is added in this change: the current evidence does
not justify its additional hoarding/rebalancing complexity. A later diagnostic
should add sampled domain-lock wait counters before reconsidering that phase.

## Reproduction And Evidence

Tracked scripts:
`docs/experiments/scripts/scalable-nvpage-pool-20261031/`.

Raw ISO, ELF, serial logs, fio JSON, `results.csv`, and e2fsck logs are stored
outside Git at:
`/home/zn/asterinas-build-env/scalable-nvpage-pool-20261031/`.
