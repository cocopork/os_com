# rNVPCe Range Transaction Sweep (2026-11-01)

## Conclusion

The range-write-transaction branch completed all 36 rNVPCe points: random and
sequential writes across the block-size and thread sweeps. Every point wrote the
expected 2 GiB, returned successfully from `sync`, unmounted successfully, and
passed read-only `e2fsck`.

The large-request fio path is substantially faster than the earlier per-page
implementation. The random-write block-size sweep peaks at 3287.32 MiB/s and the
first-write sequential sweep peaks at 2760.11 MiB/s, both at 512 KiB. This
confirms that a large write is no longer forced through 32 independent foreground
transactions at 128 KiB.

Two bottlenecks remain visible:

1. First-write sequential fio bandwidth reaches 2760.11 MiB/s, but durable
   bandwidth peaks at only 371.69 MiB/s. Each point spends another 4.50-5.46 s in
   the final `sync`, so allocation, migration, or publication work is still
   deferred beyond the fio write path.
2. Thread scaling is limited. Random 4 KiB write peaks at 333.17 MiB/s with 24
   jobs and falls to 295.10 MiB/s at 32 jobs. First-write 4 KiB peaks at 281.92
   MiB/s with 12 jobs and falls continuously to 130.70 MiB/s at 32 jobs.

## Configuration

- Source branch: `recovery/rnvpce-range-write-transactions`
- Source commit: `66f21404a35ca384cf7a4b2de7ea61010a5a16d5`
- Target: `/dev/nvme1n1p1`, reformatted for every point
- VM: 64 vCPU, 8 GiB, virtio-blk queue depth 1024, existing CPU isolation
- NVDIMM pool: 128 MiB (64 MiB metadata and 64 MiB data), fast simulation mode
- fio: buffered `psync`, `direct=0`, `iodepth=1`, 2 GiB per point
- Thread sweep: 4 KiB, 1/4/8/12/16/20/24/28/32 jobs
- Block-size sweep: one job, 4/8/16/32/64/128/256/512/1024 KiB
- ISO SHA256: `2be7f4ce88bade9eb56ca87e6983a1f9bac519725f6bda9a2681b40e6d630bf7`
- ELF SHA256: `2ad19b63425c1b920d11a4ad351c42febcd5a8ac3fcd061722b2f259028b9c94`

This run intentionally covers the changed rNVPCe+rTIO mode only. EXT4 and rNVPC
WAL were not rebuilt or rerun because this branch does not change their paths.

## Random Write

### Thread Sweep (4 KiB)

| Jobs | fio MiB/s | sync-after MiB/s | Mean latency (us) | P99 (us) |
|---:|---:|---:|---:|---:|
| 1 | 317.08 | 288.45 | 11.92 | 22.14 |
| 4 | 106.91 | 102.55 | 142.85 | 166.91 |
| 8 | 198.95 | 186.01 | 153.32 | 226.30 |
| 12 | 268.56 | 248.48 | 171.51 | 264.19 |
| 16 | 322.47 | 288.05 | 189.30 | 272.38 |
| 20 | 323.55 | 288.54 | 238.99 | 593.92 |
| 24 | 333.17 | 294.37 | 272.13 | 1302.53 |
| 28 | 309.04 | 275.47 | 321.43 | 1892.35 |
| 32 | 295.10 | 256.32 | 392.99 | 1581.06 |

### Block-size Sweep (1 Job)

| BS | fio MiB/s | sync-after MiB/s | Mean latency (us) | P99 (us) |
|---:|---:|---:|---:|---:|
| 4 KiB | 317.22 | 284.44 | 11.93 | 23.42 |
| 8 KiB | 680.17 | 576.90 | 11.11 | 18.56 |
| 16 KiB | 1187.25 | 878.97 | 12.79 | 23.17 |
| 32 KiB | 1443.27 | 966.04 | 21.18 | 74.24 |
| 64 KiB | 1596.26 | 1107.03 | 38.42 | 1253.38 |
| 128 KiB | 2677.12 | 1452.48 | 45.48 | 806.91 |
| 256 KiB | 1959.81 | 1233.73 | 125.13 | 2572.29 |
| 512 KiB | 3287.32 | 1665.04 | 146.15 | 1056.77 |
| 1024 KiB | 2160.34 | 1462.86 | 446.87 | 3129.34 |

## First Sequential Write

### Thread Sweep (4 KiB)

| Jobs | fio MiB/s | sync-after MiB/s | Mean latency (us) | P99 (us) |
|---:|---:|---:|---:|---:|
| 1 | 210.98 | 133.42 | 18.20 | 35.58 |
| 4 | 137.12 | 96.97 | 87.27 | 175.10 |
| 8 | 233.68 | 132.64 | 120.64 | 205.82 |
| 12 | 281.92 | 148.91 | 156.04 | 261.12 |
| 16 | 218.22 | 138.85 | 275.45 | 485.38 |
| 20 | 189.08 | 138.49 | 400.95 | 733.18 |
| 24 | 171.67 | 129.36 | 530.03 | 1056.77 |
| 28 | 147.38 | 116.47 | 692.12 | 1761.28 |
| 32 | 130.70 | 103.85 | 935.55 | 1974.27 |

### Block-size Sweep (1 Job)

| BS | fio MiB/s | sync-after MiB/s | Mean latency (us) | P99 (us) |
|---:|---:|---:|---:|---:|
| 4 KiB | 221.81 | 138.01 | 17.30 | 29.31 |
| 8 KiB | 443.39 | 197.87 | 17.32 | 25.22 |
| 16 KiB | 754.05 | 236.76 | 20.40 | 38.14 |
| 32 KiB | 1268.11 | 289.27 | 24.25 | 52.99 |
| 64 KiB | 1660.99 | 322.52 | 37.00 | 58.11 |
| 128 KiB | 2293.39 | 333.01 | 53.33 | 112.13 |
| 256 KiB | 2467.47 | 340.20 | 99.04 | 864.26 |
| 512 KiB | 2760.11 | 371.69 | 176.23 | 1204.22 |
| 1024 KiB | 2275.56 | 313.63 | 427.43 | 3391.49 |

Bandwidth labeled `sync-after` is calculated from fio start until the final
`sync` returns. It therefore includes all delayed migration and persistence work.

## Data Quality

- The deliberately duplicated 4 KiB, one-job points agree closely. Random-write
  fio bandwidth differs by 0.04%; first-write bandwidth differs by 5.14%.
- All 36 fio logs contain the exact expected byte count and successful completion
  markers. All 36 corresponding e2fsck logs end with status 0.
- The 4-job dip appears in both random and first-write thread sweeps. A single run
  per point is sufficient for this development scan but not for a paper result;
  repetition with confidence intervals is required before treating it as a stable
  scaling feature.
- Raw logs and generated CSV/Markdown tables are under
  `/home/zn/asterinas-build-env/preliminary-performance-sweep-20261101-range-transactions/`.
