# rNVPCe PageCache Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make PageCache the rNVPCe hot data index, retain clean NVPages after migration, and reclaim them in scalable batches without weakening persistent extent/redirect authority.

**Architecture:** Each EXT4 inode owns an RCU-readable sparse page index whose reference-counted slots can name a clean DRAM page, dirty NVPage, or clean NVPage. Persistent extent and redirect metadata remain the crash-recovery authority. Page transitions use per-slot coordination; reclaim and allocation stay partitioned by worker/allocation domain, and no index or state lock spans copy, persistence, I/O, or waiting.

**Tech Stack:** Rust kernel code, Asterinas XArray/PageCache, EXT4/rNVPCe/rTIO, ktests, Docker/OSDK, QEMU fio.

**Spec:** [`docs/os_com_implementation/os_com_refactor_plan/025-rnvpce-pagecache-integration/README.md`](../../os_com_implementation/os_com_refactor_plan/025-rnvpce-pagecache-integration/README.md)

## Global Constraints

- PageCache is volatile acceleration; persistent extent/redirect state remains authoritative after crash.
- Do not introduce one global lock, queue, or map on every read/write/reclaim operation.
- Use per-inode indexes, per-page-slot transitions, worker-local queues, and allocation-domain grouping.
- Never hold an XArray writer lock, page-slot lock, redirect lock, or allocation-domain lock across user copy, page copy, NVM persistence, SSD I/O, or sleep.
- Preserve full-page overwrite without an SSD read and batch adjacent cold reads.
- A clean NVPage is reclaimable only after SSD authority is durable; a dirty NVPage is never discarded.
- Add failing tests before each behavior change and commit each passing task independently.

### Task 1: Heterogeneous rNVPCe Page Slots

**Files:**
- Create: `kernel/src/fs/fs_impls/ext4/inode/rnvpce_page_cache.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/rnvpce_page_cache.rs`

**Interfaces:**
- Produce `RnvpcePageCache`, `RnvpcePageSlot`, and backing states `CleanDram`, `DirtyNv`, `CleanNv`, and transient ownership.
- Provide RCU lookup, insert-if-absent, compare-and-replace, compare-and-remove, and bounded range iteration.

- [ ] Add tests for lock-free lookup, single loader ownership, same-page replacement, stale replacement rejection, and independent-page progress.
- [ ] Implement the smallest per-inode XArray page-slot index that passes the tests.
- [ ] Verify no page-index lock is held while waiting or copying.
- [ ] Run focused ktests and commit.

### Task 2: Read Path And DRAM Cold-Read Fill

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`

**Interfaces:**
- Consume Task 1 page slots before any extent/redirect lookup.
- Produce batched SSD miss fill into `CleanDram` slots and direct NV reads for `DirtyNv`/`CleanNv`.

- [ ] Add tests proving a cache hit performs zero mapping lookups and concurrent misses issue one load.
- [ ] Route rNVPCe reads through the page-slot index.
- [ ] Reuse PageCache batch BIO construction for contiguous SSD misses and readahead.
- [ ] Verify error/short-read cleanup removes only the exact loading owner.
- [ ] Run focused read tests and commit.

### Task 3: Write Transitions And Persistent Publication

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produce `CleanDram -> DirtyNv`, `CleanNv -> DirtyNv`, and `DirtyNv -> DirtyNv` transitions.
- Consume existing range-write transaction and persistent redirect publication protocols.

- [ ] Add tests for full-page overwrite without SSD read, partial overwrite preservation, concurrent same-page writers, and publication failure rollback.
- [ ] Implement transitions with persistence before PageCache pointer publication.
- [ ] Batch contiguous range transitions without per-page tree routing.
- [ ] Remove hot-read dependence on the exact PageKey redirect lookup while retaining recovery/migration indexes.
- [ ] Run write, crash-cut, and redirect integrity tests; commit.

### Task 4: Clean NVPage Completion And Delayed Reclaim

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/reclaim.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produce `DirtyNv -> CleanNv` after durable SSD authority.
- Produce hot `CleanNv -> CleanDram` and cold `CleanNv -> absent` reclaim paths.

- [ ] Add tests proving completion retains a readable NVPage and dirty/in-flight pages cannot be reclaimed.
- [ ] Stop immediate completion-side slot release and publish `CleanNv` into the matching page slots.
- [ ] Implement worker/allocation-domain batched reclaim with exact compare-and-replace/remove.
- [ ] Make foreground capacity assistance request an adaptive batch rather than one page per 4 KiB write.
- [ ] Run reclaim, recovery, sync, and unmount tests; commit.

### Task 5: Recovery, Verification, And Performance

**Files:**
- Modify: `docs/os_com_implementation/rNVPC-rTIO/implementation.md`
- Create: `docs/experiments/reports/RNVPCE-PAGECACHE-INTEGRATION-2026-11-18.md`

- [ ] Verify recovery starts with empty PageCache and reconstructs authority only from persistent extent/redirect state.
- [ ] Run formatting, normal compile, ktest compile, focused ktests, and crash/fault tests.
- [ ] Build a release image and run read/write/sync/unmount/e2fsck smoke tests.
- [ ] Measure NVMe cold and hot random reads, 4 KiB overwrite thread sweep, and 128 KiB sequential overwrite.
- [ ] Measure SATA with one effective hardware queue and record queue/worker configuration.
- [ ] Compare extent-route counts, PageCache hit rate, clean-NV retention, reclaim batch size, capacity wait, throughput, and latency against the pre-change main baseline.
- [ ] Update implementation status with actual results and commit.
