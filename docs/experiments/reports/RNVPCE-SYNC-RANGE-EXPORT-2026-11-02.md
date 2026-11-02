# rNVPCe Sync Range Export (2026-11-02)

## Conclusion

The rNVPCe native-baseline export path now publishes one contiguous SSD extent
through one range transaction instead of calling the extent installer once per
4 KiB page. A kernel regression confirms that an eight-page SSD extent uses one
install transaction and retains the expected contiguous logical-to-physical
mapping.

Two targeted 2 GiB first-write points show that this removes the dominant final
`sync` delay. The 128 KiB point reduced the post-fio sync interval from 4.93 s to
0.10 s; the 512 KiB point reduced it from 4.50 s to 0.09 s. Both runs completed
the requested byte count, unmounted successfully, and passed read-only
`e2fsck`.

| BS | Version | fio MiB/s | sync-after MiB/s | Post-fio sync |
|---:|---|---:|---:|---:|
| 128 KiB | Per-page export | 2293.39 | 333.01 | 4.93 s |
| 128 KiB | Range export | 1865.21 | 1264.20 | 0.10 s |
| 512 KiB | Per-page export | 2760.11 | 371.69 | 4.50 s |
| 512 KiB | Range export | 3070.46 | 1932.08 | 0.09 s |

`sync-after` covers fio start through the final `sync` return. Relative to the
previous scan, durable throughput improved by 3.80x at 128 KiB and 5.20x at
512 KiB. The 128 KiB fio-phase difference is a single-run variation and is not
evidence of a foreground regression; this experiment targets the final sync
path rather than producing paper-quality confidence intervals.

## Implementation

- The SSD extent visitor now carries `(logical, physical, length)` mappings and
  batches up to 64 extents, rather than expanding each extent into 64 blocks.
- The range installer takes the inode extent-tree lock once, validates every
  existing overlap before editing, accepts correct already-published mappings,
  converts matching unwritten ranges, and inserts maximal hole ranges.
- Native EXT4's 32768-block written-extent encoding limit remains enforced by
  splitting only ranges that exceed that format limit.
- rTIO migration threading and ordering are unchanged.

## Verification

- `cargo osdk check --package aster-kernel`: passed.
- `cargo osdk check --ktests --package aster-kernel`: passed.
- `cargo osdk test ext4_research_routes`: 1 aggregate test passed, including
  all 24 rNVPC/EXT4 scenarios.
- Target: `/dev/nvme1n1p1`, reformatted before each point.
- VM: 64 vCPU, 8 GiB, virtio-blk queue depth 1024, one queue.
- rNVPC pool: 128 MiB, with 64 MiB metadata reserve and 64 MiB data limit.
- fio: buffered `psync`, one job, 2 GiB.
- ISO SHA256:
  `766c6f5952943ab6938fe8e658a4fdb926f347fe645edcba75d61d25f2a631ab`.

Validated raw logs and filesystem checks are stored outside Git under:

`/home/zn/asterinas-build-env/rnvpce-sync-range-export-20261102-v2/`

The comparison baseline is the complete scan in
`RNVPCE-RANGE-TRANSACTION-SWEEP-2026-11-01.md`.
