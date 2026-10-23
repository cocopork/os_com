# Extent Migration Lease Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rTIO claim and migrate a complete NV extent under one stable lease while foreground writes perform capacity admission and migration waits before mutation.

**Architecture:** Add an extent-scoped lease keyed by `(inode, leaf_slot, entry_index)`. It owns one leaf mutation guard, a stable snapshot of the seed NV entry, an inode-wide migration gate required by the current lock order, and the extent's page eviction guards across asynchronous SSD I/O. Split foreground admission into preflight reservation and locked revalidation, with every wait occurring after inode, publication, extent and capacity resources are released.

**Tech Stack:** Rust/no_std kernel code, rNVPCe soft updates, EXT4 migration worker, OSDK ktests in Docker/QEMU.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/016-extent-migration-lease/README.md`

## Global Constraints

- Candidate and page-claim unit is one complete NV extent entry. The leaf mutation guard remains the physical serialization primitive.
- No SSD I/O, NVDIMM persist or wait while holding a spinlock.
- Completion must not acquire inode publication while retaining a leaf mutation guard.
- Foreground capacity and migration waits release inode, publication, extent and reservation state first.
- Crash recovery continues to choose authority from the durable extent target; the lease remains DRAM-only.

## Review Focus

- Partial page-claim failure must restore every earlier page to `NvDirty` and requeue the extent.
- A foreground overwrite racing an active lease must retry without a partial data or extent publication.
- Mapping changes between preflight and locked revalidation must not under-reserve or leak reservation pages.
- Completion errors before target publication must leave the complete NV extent reachable and retryable.
- Completion after target publication must not require publication reacquisition or a movable entry index.

---

### Task 1: Extent Lease State Machine

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/extent_mutation.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Produces: `ExtentMigrationLease`, an extent snapshot accessor, and all-or-nothing extent claim/abort behavior.
- Consumes: existing `MetadataMutationGuard`, `ExtentRangeFreeze`, `EvictingDataPage`, `ExtentRecordEntry` and reverse locators.

- [x] Add a failing ktest proving one multi-page claim owns the complete entry and a second overlapping claim is rejected.
- [x] Add a failing ktest proving failure on a middle page rolls back all earlier page guards and the range freeze.
- [x] Implement leaf claim with one guarded read, stable entry snapshots, range freezes and page guard vectors.
- [x] Run the focused rNVPC extent migration ktests in Docker/QEMU and confirm both pass.

### Task 2: Extent-Granular Candidate and Completion

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/extent_mutation.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `ExtentMigrationLease` from Task 1.
- Produces: extent-level candidate claim and guard-based single-target publication without normal-path revalidation.

- [ ] Add a failing ktest asserting a 32-page NV extent performs one leaf validation and one target publication.
- [x] Replace page regroup/revalidation with extent claim; preserve whole-extent retry on any claim failure.
- [x] Add guard-based SSD target publication that consumes the lease snapshot and never reacquires publication.
- [x] Remove normal-path calls to `migration_extent_length` and `validate_migration_and_reverse` from candidate/completion.
- [ ] Run focused migration and crash-point ktests in Docker/QEMU.

### Task 3: Foreground Admission and Migration Retry

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`

**Interfaces:**
- Consumes: extent freeze visibility and wake generation from Task 1.
- Produces: preflight reservation, locked revalidation result and wait/retry loop with no held mutation resources.

- [ ] Add a failing ktest proving capacity shortage does not enter extent mutation or publish a partial write.
- [ ] Add a failing ktest proving an overlapping active lease makes foreground release its guards and retry after wake.
- [x] Move capacity reservation before the inode/publication mutation section and revalidate required pages after locking.
- [x] Convert an `InFlight` page/frozen extent conflict into the same release-wait-retry path.
- [ ] Run focused rNVPCe write, capacity-pressure and concurrent migration ktests in Docker/QEMU.

### Task 4: Verification and Measurement

**Files:**
- Modify: `docs/os_com_implementation/os_com_refactor_plan/016-extent-migration-lease/README.md`
- Create: `docs/experiments/reports/EXTENT-MIGRATION-LEASE-2026-10-22.md`

**Interfaces:**
- Consumes: Tasks 1-3 implementation and existing latency probes.
- Produces: build, correctness, crash and real-device performance evidence.

- [ ] Run formatting, production build and complete kernel ktest suite in the project Docker image.
- [ ] Run claim/I/O/target/cleanup crash-point tests and confirm only complete NV or SSD authority is visible.
- [x] Run the established 2 GiB, 64 MiB/32 MiB fio matrix for 4 KiB random and 128 KiB sequential writes.
- [x] Record leaf validation count, candidate/completion time, publication wait and bandwidth in the report.
- [x] Update README status and the refactor-plan index from measured evidence.
