# NVM Allocation Metadata Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rNVPCe block allocation metadata NVM-authoritative and remove all synchronous SSD metadata I/O from the rTIO migration flush path.

**Architecture:** Persistent per-group directory records select either an SSD writeback or a two-chunk NVM block-bitmap image. Allocation publishes bitmap chunks before the directory generation, recovery overlays selected NVM images onto the DRAM allocator, and a bounded pager asynchronously writes back and evicts groups. rTIO submits only asynchronous data BIOs; completion retains the single data durability flush.

**Tech Stack:** Rust `no_std`, Asterinas kernel, rNVPCe persistent metadata, EXT4 block-group allocator, asynchronous BIOs, OSDK/QEMU ktests.

**Spec:** `docs/superpowers/specs/2026-10-28-nv-allocation-metadata-cache-design.md`

## Global Constraints

- NVM is authoritative for every `ResidentDirty` allocation group after a crash.
- `DIRTY` and `WRITEBACK` are DRAM state bits; persistent `generation` selects NVM object versions, and no incrementing mutation epoch is used.
- One block group has at most one data-migration or allocation-metadata-writeback owner; disjoint groups may proceed independently.
- rTIO target preparation and flush must call no synchronous block-device read, write, flush, or `IoBatch::wait_all` operation.
- The first implementation covers block bitmap, GDT allocation fields/checksum, and superblock free-block count only.
- No block-device I/O or sleep may occur while holding an rNVPC spinlock, block-group metadata lock, or superblock lock.
- Bitmap chunks become reachable before descriptor targets, and SSD writeback authority becomes visible only after I/O completion plus device flush.
- Native EXT4 and rNVPC WAL allocation behavior must remain unchanged.
- Existing `AllocationRecord` remains a per-inode allocation-credit record; it is not repurposed as a bitmap image.
- Older persistent images are rejected after the layout version change; no implicit conversion is added.

## Review Focus

- Crash after one bitmap chunk but before directory publication must preserve the old selected generation; Task 1 adds selector-boundary tests.
- A redirtied writeback must not hand authority to SSD; Task 4 adds DIRTY/WRITEBACK state tests. Exact generation still rejects old completions.
- Recovery must reject a resident directory with a missing/corrupt chunk instead of falling back to SSD; Task 2 adds corruption tests.
- Cache exhaustion must stop or defer a migration round without performing synchronous SSD I/O under the migration lock; Task 4 adds a one-slot pressure test.
- Explicit sync must leave an independently mountable EXT4 image even when dirty groups were resident in NVM; Task 5 adds sync/remount and e2fsck coverage.

---

## File Structure

- Create `kernel/src/fs/rnvpc/allocation_metadata.rs`: persistent directory/chunk formats, authority transitions, cache state, snapshot tokens, and recovery scan helpers.
- Modify `kernel/src/fs/rnvpc/metadata.rs`: add allocation-directory and bitmap-chunk metadata kinds and validation.
- Modify `kernel/src/fs/rnvpc/frame.rs`: own the allocation metadata cache and expose pool-facing publication/recovery methods.
- Modify `kernel/src/fs/rnvpc/recover.rs`: retain reachable allocation metadata generations and reclaim incomplete chunk publications.
- Modify `kernel/src/fs/fs_impls/ext4/block_group.rs`: export/import complete block allocation images without I/O and apply NVM-authoritative state.
- Modify `kernel/src/fs/fs_impls/ext4/fs.rs`: initialize/recover the cache, route rNVPCe allocation through it, and recompute aggregate counters.
- Modify `kernel/src/fs/fs_impls/ext4/migration.rs`: remove synchronous metadata writes and defer when no allocation group is resident.
- Create `kernel/src/fs/fs_impls/ext4/allocation_pager.rs`: asynchronous writeback/load scheduling, immutable BIO buffers, completion, and eviction.
- Modify `kernel/src/fs/fs_impls/ext4/mod.rs`: register the pager module.
- Modify `kernel/src/fs/fs_impls/ext4/nv_tests.rs` and `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`: crash, recovery, cache-pressure, and end-to-end tests.
- Modify pipeline metrics and experiment scripts/reports only after correctness is green.

### Task 1: Define Crash-Consistent Allocation Group Records

**Files:**
- Create: `kernel/src/fs/rnvpc/allocation_metadata.rs`
- Modify: `kernel/src/fs/rnvpc/metadata.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produces: `AllocationGroupState`, `AllocationGroupDirectoryRecord`, `AllocationBitmapChunkRecord`, `AllocationGroupImage`, and `AllocationMetadataCache::{publish_initial, publish_update, selected_image}`.
- Persistent bitmap chunk payload is exactly half of the EXT4 4 KiB bitmap; both chunks carry filesystem/group/generation/chunk identity and checksum.

- [ ] **Step 1: Write failing record-layout and crash-boundary tests**

  Add `rnvpce_allocation_bitmap_requires_two_matching_chunks`, `rnvpce_allocation_directory_selects_only_complete_generation`, `rnvpce_allocation_update_crash_keeps_old_generation`, and `rnvpce_allocation_directory_rejects_corrupt_selected_chunk`. Assert exact object sizes, identity validation, old-generation selection before directory publication, and `EUCLEAN` for a corrupt selected image.

- [ ] **Step 2: Run the focused tests and verify missing-type failures**

  Run: `cargo osdk test rnvpce_allocation_ --package aster-kernel -- --nocapture`

  Expected: FAIL because the allocation metadata record types do not exist.

- [ ] **Step 3: Implement typed records and two-copy directory publication**

  Add new metadata kinds and validation. Store each bitmap as two fixed-size chunk records. Publish new chunks first and switch the directory through the existing persistent object replacement mechanism; never mutate the selected generation before its replacement is durable.

- [ ] **Step 4: Implement incomplete-generation reclamation rules**

  Make only directory-selected chunks reachable during rNVPC recovery. Preserve the previous directory generation until the selector replacement commits; reclaim unreferenced chunks after the selected cache state is reconstructed.

- [ ] **Step 5: Run focused tests and ktest typecheck**

  Run: `cargo osdk test rnvpce_allocation_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Expected: focused tests pass and ktest compilation exits 0.

### Task 2: Restore NVM-Authoritative Images Into the EXT4 Allocator

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/recover.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `AllocationMetadataCache::selected_images(fs_id)` from Task 1.
- Produces: `BlockGroup::{allocation_image, install_authoritative_allocation_image}` and `Ext4::restore_rnvpce_allocation_metadata()`.

- [ ] **Step 1: Write failing recovery-overlay tests**

  Add `rnvpce_recovery_prefers_nv_block_bitmap`, `rnvpce_recovery_recomputes_group_and_super_counts`, `rnvpce_recovery_rejects_missing_selected_bitmap_chunk`, and `rnvpce_recovery_is_idempotent`. Build an SSD image whose bitmap/counts are one generation behind NVM and assert the allocator never returns an NVM-allocated block.

- [ ] **Step 2: Run the focused tests and verify SSD state incorrectly wins**

  Run: `cargo osdk test rnvpce_recovery_prefers_nv --package aster-kernel -- --nocapture`

  Expected: FAIL because mount currently initializes allocation exclusively from SSD metadata.

- [ ] **Step 3: Add lock-contained block-group image import/export**

  Snapshot or install the block bitmap, free-block count, and checksum inputs under `BlockGroup::metadata`; perform no I/O in these methods. Validate group geometry and fixed metadata reservations before accepting an image.

- [ ] **Step 4: Overlay NVM groups before rTIO starts**

  After loading SSD block groups, install every selected resident NVM image, recompute the superblock free-block count and `block_capacity`, then reconcile pending migration targets. A missing/corrupt selected NVM image returns `EUCLEAN` without synchronously repairing SSD.

- [ ] **Step 5: Run recovery tests and existing pending-target tests**

  Run: `cargo osdk test rnvpce_recovery_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_pending_target --package aster-kernel -- --nocapture`

  Expected: all selected tests pass.

### Task 3: Publish rTIO Allocations to NVM and Remove Sync SSD Metadata I/O

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: Task 1 cache publication and Task 2 block-group images.
- Produces: `Ext4::alloc_rnvpce_blocks(count, goal) -> Result<Option<Range<Ext4Bid>>>`; `None` means a group load/writeback was queued and the migration round must defer.

- [ ] **Step 1: Write failing soft-update and no-sync-I/O tests**

  Add `rnvpce_target_allocation_persists_nv_bitmap_before_descriptor`, `rnvpce_allocation_failure_leaves_no_descriptor_target`, `rnvpce_migration_flush_submits_no_metadata_bio`, and `rnvpce_migration_flush_calls_no_synchronous_block_io`. Instrument the test block device and assert zero sync reads, writes, flushes, and waits in target preparation/flush.

- [ ] **Step 2: Run focused tests and observe the current synchronous writes**

  Run: `cargo osdk test rnvpce_migration_flush_ --package aster-kernel -- --nocapture`

  Expected: FAIL because `sync_rnvpce_allocation_metadata` calls synchronous bitmap/GDT/superblock I/O.

- [ ] **Step 3: Add the rNVPCe allocation publication boundary**

  Allocate under the group lock, publish the complete updated allocation image to NVM, and only then assign descriptor migration targets. Rollback publishes a newer NVM image before releasing blocks. Keep Native/rNVPC allocation on `alloc_blocks_with_intent`.

- [ ] **Step 4: Remove allocation metadata from `MigrationBatch` readiness**

  Delete `sync_rnvpce_allocation_metadata` from `migrate_nv_round`; seal the batch after asynchronous data BIO submission. Keep exactly one data durability `BlockDevice::sync` in completion before SSD baseline publication.

- [ ] **Step 5: Run migration, crash-boundary, and typecheck tests**

  Run: `cargo osdk test rnvpce_migration_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Expected: selected tests pass; the flush-path instrumentation records no synchronous block I/O.

### Task 4: Add the Bounded Asynchronous Allocation Metadata Pager

**Files:**
- Create: `kernel/src/fs/fs_impls/ext4/allocation_pager.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/allocation_metadata.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Produces: `AllocationPager::{request_resident, request_writeback, run_once, drain}`; snapshot tokens include `(group, generation, bitmap checksum)` and complete only the exact generation.
- `request_resident` and `request_writeback` never perform block I/O and may be called under migration coordination after block-group locks are released.

- [ ] **Step 1: Write failing pressure and stale-completion tests**

  Add `rnvpce_pager_evicts_full_group_before_free_group`, `rnvpce_pager_does_not_handoff_redirtied_group`, `rnvpce_pager_releases_slots_only_after_flush`, `rnvpce_cache_pressure_defers_migration_without_sync_io`, and `rnvpce_pager_failure_wakes_migration_waiters` with a cache limited to one resident group.

- [ ] **Step 2: Run focused tests and verify the pager API is absent**

  Run: `cargo osdk test rnvpce_pager_ --package aster-kernel -- --nocapture`

  Expected: FAIL because the pager and bounded residency policy do not exist.

- [ ] **Step 3: Implement bounded residency and asynchronous writeback buffers**

  Maintain resident LRU/full-group queues in DRAM and persistent directory authority in NVM. Build immutable bitmap/GDT after-images under short locks, submit them with `write_bytes_async`, and let callbacks queue completion work without persistence or allocation.

- [ ] **Step 4: Implement writeback flush and exact-generation eviction**

  The pager worker waits outside metadata locks, issues one flush for a completed writeback batch, then changes only matching generations to `CleanOnSsd` and releases their chunks. A newer dirty generation remains resident and is queued again.

- [ ] **Step 5: Implement asynchronous residency requests and migration deferral**

  A nonresident group request schedules admission from the current validated DRAM/SSD writeback image. If slots are unavailable, schedule eviction and return pending; the migration round restores claims to dirty state and retries after the pager wakeup.

- [ ] **Step 6: Run pager tests and all rNVPCe ktests**

  Run: `cargo osdk test rnvpce_pager_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_ --package aster-kernel -- --nocapture`

  Expected: all selected tests pass.

### Task 5: Writeback on Sync, Verify the Device Image, and Measure

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/allocation_pager.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/pipeline_profile.rs`
- Modify: `kernel/src/fs/rnvpc/batch_metrics.rs`
- Modify: experiment scripts under `docs/experiments/scripts/`
- Create: a dated report under `docs/experiments/reports/`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `AllocationPager::drain` and existing `export_nv_for_sync`.
- Produces: clean-unmount writeback ordering and metrics for NVM publication, pager queue/submit/flush, cache hit/miss/eviction, and migration submission.

- [ ] **Step 1: Write failing explicit-sync and remount tests**

  Add `rnvpce_sync_writes_back_all_dirty_allocation_groups`, `rnvpce_clean_unmount_is_native_mountable`, and `rnvpce_sync_propagates_pager_io_failure`. Assert native export precedes the final allocation-metadata writeback and that an SSD-only remount sees the same allocated blocks/counts.

- [ ] **Step 2: Run focused tests and observe stale SSD allocation metadata**

  Run: `cargo osdk test rnvpce_sync_writeback --package aster-kernel -- --nocapture`

  Expected: FAIL because dirty NVM allocation images are not drained to SSD.

- [ ] **Step 3: Integrate pager drain into sync/unmount ordering**

  Drain data migration, export native mappings, write back all dirty allocation groups, and issue the final device flush before reporting sync success. Preserve dirty NVM authority and return `EIO` on any failed writeback.

- [ ] **Step 4: Add stage counters and run the full verification suite**

  Run: `cargo fmt --all -- --check`

  Run: `cargo osdk test rnvpce_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk test --package aster-kernel -- --nocapture`

  Expected: formatting and complete kernel ktests pass.

- [ ] **Step 5: Run real-device correctness and performance experiments**

  Use the established 2 GiB, 64/32 MiB watermark, CPU-isolated configuration on the authorized NVMe test partition. Require fio success, sync/unmount success, and clean `e2fsck -fn`. Compare 4 KiB random and 128 KiB sequential writes at 1/16 threads, with three runs per point.

- [ ] **Step 6: Record results without claiming unmeasured improvement**

  Report fio and drain bandwidth, all new stage times/calls, cache residency/eviction counts, BIO/flush counts, and failures. Distinguish design intent from measured evidence.
