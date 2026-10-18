# AchillesFS Implementation Reference

Current implementation status is authoritative in [`../STATUS.md`](../STATUS.md). This document preserves the completed rNVPC foreground-checkpoint comparison object; active rNVPCe development follows [`rnvpce-soft-update-implementation-plan.md`](rnvpce-soft-update-implementation-plan.md).

For rNVPCe, `committed` means persistently valid, `mapped` means selected by the current inode extent tree, `reachable` describes recovery's root traversal, and `reclaim` means durable clear plus frame reuse. A committed object is not necessarily mapped or reachable.

## Source Baseline

The temporary `../os_com-rnvpc-foreground-checkpoint` worktree has been removed. Its completed implementation is preserved in `../os_com` history, while active WAL-free rNVPCe development proceeds on branch `rnvpce-soft-update`. The completed comparison-object series runs from persistent layouts through recovery and validation; the final commits are:

- `3f1946a83`: foreground full-WAL checkpoint.
- `4711e88de`: rTIO completion decoupled from WAL retirement.
- `d0e89bf65`..`28a4abc6a`: persistent WAL/B-link recovery and retry-safe cleanup.
- `1bd8eadd0`: stable checkpoint counters.
- `fc2c1735c`: recovery-only candidate scans removed from the clean checkpoint path.
- `f0184385d`: image-backed foreground-checkpoint smoke workload.

## Runtime Path

For an `rnvpc=on,rtio=on` mount, each foreground page update persists a new NV data page, appends and commits one persistent WAL record, advances the WAL anchor, then publishes the DRAM mapping. The per-inode WAL holds 1024 records. The write that needs record 1025 synchronously replays the preceding 1024 records in LSN order into the persistent per-inode B-link B+tree, publishes any new root through the anchor, advances the checkpointed LSN, reclaims replaced pages, and then appends its own record. There is no background WAL-checkpoint thread.

rTIO may flush only pages no longer pinned by the WAL. SSD completion removes the exact B+tree mapping and reclaims the matching NV generation, but does not advance or retire the WAL. Different inodes use independent mutation locks; one inode's checkpoint is serialized.

## Recovery

Recovery scans self-describing persistent descriptors, reconstructs each inode's anchor, WAL frames, and B-link tree, replays committed records beyond the checkpointed LSN, publishes repaired roots, rebuilds the DRAM acceleration map, and reclaims only unreachable committed objects. Split-candidate scans are enabled only during recovery replay and are disabled before the recovered context is exposed.

## Observability

The existing `[RNVPC]` summary exports `wal_appends`, `checkpoint_count`, `checkpoint_records`, `btree_updates`, `btree_splits`, `persist_fences`, `checkpoint_blocked_writes`, and `wal_max_used`. Event counters are always enabled; normal runs do not add timer reads. rNVPCe remains WAL-free and reports these fields as zero.

## Verification

The following gates passed on the committed branch:

```bash
cd kernel
CARGO_NET_OFFLINE=true cargo osdk check --package aster-kernel
CARGO_NET_OFFLINE=true cargo osdk check --ktests --package aster-kernel
cd ..
CARGO_NET_OFFLINE=true make ktest ENABLE_KVM=1
```

The full KVM run executed all 800 `aster-kernel` tests with zero failures. The image-backed smoke completed 8 MiB random 4 KiB writes and 256 MiB sequential 128 KiB writes and emitted `RNVPC_FOREGROUND_SMOKE_COMPLETE`. It used `rnvpc_size=1G`, `rnvpc_meta_reserve=128M`, and `rnvpc_data_limit=768M`; the larger data limit accommodates temporary duplicate foreground/writeback NV versions and is not a performance result.

## Limitations

- The durable image is modeled with DRAM. Tests validate ordering, restart reconstruction, fault handling, and reachability, but do not prove real-NVDIMM power-loss behavior.
- This WAL/B-link implementation is a completed comparison object, not the active rNVPCe publication protocol.
- Sequential smoke throughput is low and remains a separate integration/performance issue; this task's acceptance is correctness and bounded completion.
- Formal performance conclusions still require retained experiment packages using `busy_bw_mib_s`.

## Active rNVPCe Task 6

The WAL-free `rnvpc_extend` path feeds rTIO from a duplicate-suppressed global
dirty-NVPage queue. Each candidate carries an exact allocation sequence and an
8-byte DRAM reverse locator for its current extent leaf slot, so normal
candidate discovery, reference validation, SSD publication, and reclaim do not
walk an inode extent tree. A split freezes only its moved logical interval until
the forward tree and reverse locators agree. Foreground direct copies and rTIO
use the same NVPage lease; contention waits outside the inode lock and reparses
the current mapping after wakeup.

rTIO allocates concrete SSD blocks only after selecting a physical-view batch,
persists each transient target in the existing NVPage descriptor, orders pages
by target block, and merges physically adjacent pages even across inodes. It
submits asynchronously and continues until the BIO/page budget is full. BIO
callbacks only transfer the owned completion payload into a bounded queue and
wake a dedicated completion thread. That thread materializes completed mappings
in native EXT4, commits through JBD2 or nojournal metadata sync, performs the
durability boundary, changes each persistent extent from its exact NV reference
to SSD, consumes reservation credit once, and reclaims that NV generation.
Failed I/O retains the durable target and NV mapping for retry.

Fresh production and ktest-configuration compilation pass, as does the 7-case
static IRQ/completion-context suite. Two focused TCG VM ktests pass in separate
runs: one covers reverse-index publication, retry-safe SSD conversion, and
single reservation consumption; the other injects persistent-ledger failure and
proves that live reservation credit is still released exactly once. Source-level
tests additionally cover reservation, split-range freezing, recovery rebuild,
and dirty-queue reconstruction. KVM is unavailable in this session, so the new
architecture does not inherit earlier Task 6 KVM claims; image-backed
over-capacity fio and mount/unmount validation remain pending.

Post-publication rNVPCe resize is deliberately `EOPNOTSUPP`: native truncate
would free SSD blocks still named by the persistent extent tree. Proper truncate
requires a persistent suffix-deletion transaction and remains separate follow-up
work. Formal Task 6 evaluation also remains pending until the image-backed fault,
pool-full/reuse, and retained FIO `busy_bw_mib_s` matrices are complete.
