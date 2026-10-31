# rNVPCe first-write and overwrite performance, 2026-10-30

## Scope

This experiment compares first writes with overwrites on the current
`recovery/rnvpce-page-redirection` implementation. It uses the development
validation configuration rather than the full paper configuration:

- source base commit: `35a1f3152e320c318a544705f109ed4632877c73`;
- the ISO was built from the current working tree, which also contained the
  pre-existing uncommitted changes in `ext4/fs.rs`,
  `ext4/migration/reclaim.rs`, and `rnvpc/mode.rs`;
- guest: 64 vCPUs, 8 GiB DRAM;
- rNVPC pool: 128 MiB, including a 64 MiB metadata reserve and a 64 MiB data limit;
- target: `/dev/nvme1n1p1`, recreated as 4 KiB no-journal EXT4 before every point;
- fio: 2 GiB total, `psync`, buffered I/O, queue depth 1 per job;
- workloads: 4 KiB random write and 128 KiB sequential write, at 1 and 16 jobs;
- virtio-blk: one 1024-entry queue with request merging disabled.

For an overwrite point, native EXT4 direct I/O first writes the complete set
of target files. The timed phase then remounts the file system in rNVPCe mode
and overwrites the same logical range. Preparation time is not included in the
reported bandwidth. A first-write point starts with unallocated target files.

`fio_mib_s` ends when fio returns. `durable_mib_s` additionally includes the
following `sync`, so it exposes deferred migration work left by the foreground
write path.

## Results

| Workload | Jobs | Write type | fio MiB/s | Durable MiB/s | fio time | post-fio sync |
|---|---:|---|---:|---:|---:|---:|
| 4 KiB random | 1 | first | 13.67 | 12.84 | 150.19 s | 9.32 s |
| 4 KiB random | 1 | overwrite | 218.92 | 204.19 | 9.78 s | 0.25 s |
| 4 KiB random | 16 | first | 139.15 | 87.97 | 15.14 s | 8.14 s |
| 4 KiB random | 16 | overwrite | 175.21 | 163.71 | 12.04 s | 0.47 s |
| 128 KiB sequential | 1 | first | 544.25 | 241.22 | 4.07 s | 4.42 s |
| 128 KiB sequential | 1 | overwrite | 533.06 | 452.10 | 4.28 s | 0.25 s |
| 128 KiB sequential | 16 | first | 396.28 | 193.21 | 5.65 s | 4.95 s |
| 128 KiB sequential | 16 | overwrite | 291.70 | 263.92 | 7.44 s | 0.32 s |

## Findings

1. The dominant regression is the single-job first-write path for 4 KiB
   random writes. Overwrite foreground bandwidth is 16.0 times higher, and
   durable bandwidth is 15.9 times higher. This isolates the remaining cost
   to first creation/allocation and publication work, rather than ordinary
   NVPage data replacement.
2. Sixteen jobs hide much of that first-write latency, but do not eliminate
   deferred work. The first-write point still spends 8.14 seconds in the final
   `sync`, compared with 0.47 seconds for overwrite.
3. For 128 KiB sequential writes at one job, foreground bandwidth is nearly
   identical for first write and overwrite. The first-write path nevertheless
   leaves 4.42 seconds of migration work for `sync`, reducing durable bandwidth
   from 452.10 to 241.22 MiB/s.
4. Increasing concurrency from 1 to 16 jobs does not improve 128 KiB
   throughput. It reduces foreground bandwidth for both write types, so the
   large-request path still has a shared-resource or coordination bottleneck.

All eight fio runs completed the expected 2 GiB transfer, cleanly unmounted,
and passed read-only `e2fsck` with status 0.

## Evidence

- Reproduction and validation scripts:
  `docs/experiments/scripts/rnvpce-first-overwrite-20261030/`
- Raw serial logs, fio JSON, `e2fsck` logs, and `results.csv`:
  `/home/zn/asterinas-build-env/rnvpce-first-overwrite-20261030/current/`
- ISO and kernel image hashes:
  `/home/zn/asterinas-build-env/rnvpce-first-overwrite-20261030/current/SHA256SUMS`

