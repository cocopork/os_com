# rNVPCe range write transactions (2026-10-31)

## Scope

This experiment validates the implementation described by plan 019. The Native
rNVPCe write path plans one logical range, publishes one redirect entry per
logically and physically contiguous NV range, owns its slots as one range, and
locks/persists an existing NV entry once instead of rebuilding a per-page
`PersistentObjectRef`. Requests larger than 128 pages loop over bounded range
transactions; a 1 MiB aligned write therefore completes as two 128-page
transactions rather than a short write.

Performance image source commit: `fa7af3d8c` (`Batch rNVPCe range overwrite
persistence`). The final branch additionally restores range ownership for the
legacy redirect-publication compatibility path; that change does not affect the
measured Native write path.

Release image SHA-256:

```text
adce6bb3e28d37d54ed5f7df2256ccbb01b3dafa1f32d1501697bc0fb9e4c852  selected.iso
d1d7339f8a5019ad2f575b21d915d19bbdffdc4f16009c821e6f4cbbcc17dcbf  current.elf
```

The separate diagnostic image has only
`pipeline_profile::ENABLED=true`; its SHA-256 is
`24d10c5ad445ee56716622991aee314d3742afc29f38812ce781d35a54b58757`.
The committed source keeps this switch disabled.

## Configuration

- Device: authorized `/dev/nvme1n1p1`, reformatted no-journal EXT4 before each point.
- VM: 64 vCPUs, 8 GiB RAM, one virtio-blk queue, queue size 1024.
- Pool: 128 MiB total, 64 MiB metadata reserve, 64 MiB data limit, fast NVDIMM simulation.
- Workload: 2 GiB, 128 KiB sequential buffered overwrite, `psync`, jobs 1 and 16.
- Procedure: three alternating rounds; every point performs `sync`, unmount, and host `e2fsck -fn`.
- Raw artifacts: `/home/zn/asterinas-build-env/rnvpce-range-write-20261031-final` and the corresponding `-diagnostic` directory; these are intentionally not tracked.

## Correctness

`cargo osdk check --ktests --package aster-kernel` passed. Exact tests for the
1 MiB request loop, range-owned slots, data-slot recovery, legacy redirect
publication, selector crash boundaries, stale image rejection, domain
allocation, and both rNVPC WAL recovery cases all passed under QEMU. The WAL
tests cover root-split recovery and WAL/soft-update ownership in one shared
pool.

All six fio transfers wrote exactly 2 GiB, `sync` and unmount succeeded, and all
six `e2fsck -fn` runs exited 0. The range-owner ktest also checks one range-table
lock acquisition at publication, claim, and reclaim, writer exclusion during
migration, stale-`Arc` rejection after reuse, and recovery reconstruction.

## Release Performance

| jobs | fio samples (MiB/s) | fio median | durable samples (MiB/s) | durable median |
|---:|---|---:|---|---:|
| 1 | 2701.85, 2506.73, 3016.20 | 2701.85 | 1651.61, 1452.48, 1473.38 | 1473.38 |
| 16 | 2009.81, 2432.30, 2684.14 | 2432.30 | 1077.89, 1226.35, 1442.25 | 1226.35 |

The historical comparison points were 520.19 MiB/s (jobs 1) and 288.25 MiB/s
(jobs 16). They came from an earlier implementation and should be treated as
development references, not as an A/B run under the final image.

## Structural Diagnostic

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

The legacy per-page completion stages `extent_member_validate`,
`extent_reverse_lookup`, `extent_leaf_read`, and `extent_leaf_validate` all
reported zero calls. This is the main structural result: migration handles the
524,288 pages in hundreds of batches rather than repeating tree identity work
for every page. Native overwrite still keeps per-slot `ObjectRecord` values as
a compatibility mirror for the shared allocator and recovery code, but it no
longer uses allocation sequence or logical version as the foreground identity
check. Removing that mirror entirely remains a separate cleanup, not a current
write-path dependency.

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
