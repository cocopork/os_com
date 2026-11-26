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
- A clean NVPage is reclaimable only after its SSD data and mapping are durable; a dirty NVPage is never discarded.
- Every runtime-live NVPage has an exact PageCache slot; a normal PageCache miss resolves only SSD extent metadata.
- Reclaim converts every claimed `CleanNv` page to `CleanDram` before detaching its redirect and freeing the NV range.
- Add failing tests before each behavior change and commit each passing task independently.

### Task 1: Heterogeneous rNVPCe Page Slots

**Files:**
- Create: `kernel/src/fs/fs_impls/ext4/inode/rnvpce_page_cache.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/rnvpce_page_cache.rs`

**Interfaces:**
- Produce `RnvpcePageCache`, `RnvpcePageSlot`, and backing states `CleanDram`, `DirtyNv`, `CleanNv`, and transient ownership.
- Provide RCU lookup, insert-if-absent, compare-and-replace, compare-and-remove, and bounded range iteration.

- [x] Add tests for lock-free lookup, single loader ownership, same-page replacement, stale replacement rejection, and independent-page progress.
- [x] Implement the smallest per-inode XArray page-slot index that passes the tests.
- [x] Verify no page-index lock is held while waiting or copying.
- [x] Run focused ktests and commit.

### Task 2: Read Path And DRAM Cold-Read Fill

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`

**Interfaces:**
- Consume Task 1 page slots before any extent/redirect lookup.
- Produce batched SSD miss fill into `CleanDram` slots and direct NV reads for `DirtyNv`/`CleanNv`.

- [x] Add tests proving a cache hit performs zero mapping lookups and concurrent misses issue one load.
- [x] Route rNVPCe reads through the page-slot index.
- [x] Reuse PageCache batch BIO construction for contiguous SSD misses and readahead.
- [x] Verify error/short-read cleanup removes only the exact loading owner.
- [ ] Remove per-page NV authority lookup from the miss path; use NVPool-cached SSD extents and the SSD extent tree only.
- [ ] Rebuild all NV-backed PageCache slots from persistent redirects before recovery exposes file I/O.
- [x] Run focused read tests and commit.

### Task 3: Write Transitions And Persistent Publication

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produce `CleanDram -> DirtyNv`, `CleanNv -> DirtyNv`, and `DirtyNv -> DirtyNv` transitions.
- Consume existing range-write transaction and persistent redirect publication protocols.

- [x] Add focused cache-publication and full-page no-SSD-read coverage; retain the existing partial-write/crash tests.
- [ ] Implement every direct transition with persistence before PageCache pointer publication (`CleanNv -> DirtyNv` reuse remains).
- [x] Batch contiguous range transitions without per-page tree routing.
- [x] Remove hot-read dependence on the exact PageKey redirect lookup while retaining recovery/migration indexes.
- [x] Run focused write and redirect integrity tests and commit; three pre-existing direct-transaction failures remain recorded in the SDD ledger.

### Task 4: Clean NVPage Completion And Delayed Reclaim

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/reclaim.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produce `DirtyNv -> CleanNv` after the SSD data and mapping are durable while retaining the NV redirect until reclaim.
- Produce batched `CleanNv -> CleanDram` conversion before redirect detachment and NVPage release.

- [x] Add tests proving completion retains a readable NVPage and dirty/in-flight pages cannot be reclaimed.
- [x] Stop immediate completion-side slot release and publish `CleanNv` into the matching page slots.
- [x] Implement worker/allocation-domain batched reclaim with exact compare-and-replace/remove.
- [x] Keep foreground assistance as a pressure signal while the owning worker reclaims up to the normal batch budget, rather than one page per 4 KiB write.
- [ ] Remove the cold `CleanNv -> absent` path; reserve DRAM capacity, evict older CleanDram pages if needed, and convert the entire claimed batch.
- [x] Run reclaim, recovery, sync, and unmount tests; commit.

### Task 5: Recovery, Verification, And Performance

**Files:**
- Modify: `docs/os_com_implementation/rNVPC-rTIO/implementation.md`
- Create: `docs/experiments/validation/temporary/rnvpce-pagecache-integration-regression-20261118/README.md`

- [ ] Verify recovery starts with empty PageCache and reconstructs authority only from persistent extent/redirect state.
- [ ] Run formatting, normal compile, ktest compile, focused ktests, and crash/fault tests.
- [ ] Build a release image and run read/write/sync/unmount/e2fsck smoke tests.
- [ ] Measure NVMe cold and hot random reads, 4 KiB overwrite thread sweep, and 128 KiB sequential overwrite.
- [ ] Measure SATA with one effective hardware queue and record queue/worker configuration.
- [ ] Compare extent-route counts, PageCache hit rate, clean-NV retention, reclaim batch size, capacity wait, throughput, and latency against the pre-change main baseline.
- [ ] Update implementation status with actual results and commit.
