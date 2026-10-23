# Optimistic Extent Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore optimistic foreground/migration concurrency while retaining FAST/FAIR extent persistence and complete-NV-extent batching.

**Architecture:** rTIO claims and pins one complete NV extent under a short publication interval, then releases all inode/leaf coordination before SSD target allocation and I/O. Completion reacquires publication authority, resolves the current forward edge once from the extent's logical identity, validates the complete pinned generation vector, performs one 8-byte SSD target store, and batch-reclaims the pages. Foreground writes do not use an inode-wide migration gate; an overlap with an `InFlight` generation waits outside inode/publication/leaf locks and retries.

**Tech Stack:** Rust `no_std`, Asterinas kernel ktests, OSDK/QEMU, EXT4/rNVPCe, fio on the existing NVMe test partition.

**Spec:** `docs/superpowers/specs/2026-10-22-optimistic-extent-migration-design.md`

## Global Constraints

- Preserve the committed FAST/FAIR target word, in-place entry updates, FAIR sibling routing, contiguous NV extent representation, and batched descriptor persistence.
- No inode publication guard, leaf mutation guard, range freeze, or inode-wide gate may span SSD allocation, BIO submission, device flush, or completion waiting.
- One complete contiguous NV extent is the migration and failure unit; no partial per-page target publication fallback.
- Ordinary NV-to-SSD authority change is one aligned 8-byte `ExtentTarget` persistence operation.
- `InFlight` physical generations are immutable and pinned until successful publication/reclaim or whole-extent abort.
- SSD allocations named by durable descriptors remain owned on stale completion and are reused or reconciled; do not speculatively free them.

## Review Focus

- Concurrent FAIR leaf split moves the entry after claim: completion re-resolves once and never updates the stale leaf hint.
- Foreground overlap with any member generation: the writer waits without inode/publication/leaf locks and retries without modifying `InFlight` data.
- One member generation or target changes: the whole extent aborts; no subset is published or reclaimed.
- Contiguous NV pages with fragmented SSD targets: return `EAGAIN` without changing authority or losing descriptor ownership.
- Faults immediately before and after the 8-byte target store: recovery sees complete NV or complete SSD authority and no dangling slot.

---

### Task 1: Remove The Cross-I/O Lease And Inode Gate

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/rnvpc/tests.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`

**Interfaces:**
- Consumes: committed `HEAD` behavior at `51ea0ba27` and the spec's explicit code-scope list.
- Produces: a compiling baseline with no `ExtentMigrationLease`, inode-wide `0..u64::MAX` freeze, foreground range-gate wait, or leaf guard retained across I/O.

- [x] **Step 1: Restore the six production/test files named by the spec to committed `HEAD` and inspect the resulting diff**

  Preserve unrelated `.vscode` and documentation changes. Confirm `rg 'ExtentMigrationLease|claim_extent_migration|migration_range_is_frozen'` has no remaining uncommitted lease implementation.

- [x] **Step 2: Run formatting and a kernel compile check**

  Run the repository formatter, then build the kernel in the existing Docker/OSDK environment. Expected: no compile error before adding the optimistic extent snapshot.

### Task 2: Claim A Complete Extent Without Retaining Metadata Locks

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `PersistentObjectRef`, `ReverseExtentRef`, `PageKey`, `EvictingDataPage`, and contiguous `ExtentRecordEntry::nv_slot_at`.
- Produces: `ClaimedMigrationExtent` carrying fs/inode, logical start/length, expected NV base, member references/generations, SSD targets, locator hint, and page eviction guards, but no publication or metadata guard.
- Produces: `Rnvpc::claim_migration_extent(seed, locator) -> Result<ClaimedMigrationExtent>` that validates one leaf snapshot and atomically retains all member page pins or none.

- [x] **Step 1: Add failing ktests for lock-free claim lifetime and whole-extent rollback**

  Add tests proving that a claimed 32-page extent pins all exact generations, contains no range freeze, permits an unrelated foreground publication on the same inode, and returns every member to `NvDirty` when the claim is dropped before publication.

- [x] **Step 2: Run the focused `rnvpce_` ktest filter and confirm the new tests fail**

  Use the existing Docker/OSDK harness and retain the failing output as implementation evidence.

- [x] **Step 3: Implement `ClaimedMigrationExtent` and `claim_migration_extent`**

  Acquire publication/leaf coordination only while resolving and validating the entry. Build the full member vector and pin exact allocation generations; release all metadata coordination before returning. On any member failure, RAII drops the accumulated evictions and the caller requeues the complete extent.

- [x] **Step 4: Change candidate selection and `migrate_nv_round` to one seed and one claim per extent**

  Deduplicate by `(inode, leaf_slot, entry_index)`, account the complete extent against the page budget, transfer its member evictions into `PendingPage`, and publish SSD target identities without retaining claim-time metadata guards.

- [x] **Step 5: Run focused ktests and compile**

  Expected: claim/rollback/concurrency tests pass and the kernel builds.

### Task 3: Re-resolve And Publish Once Per Extent At Completion

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: `ClaimedMigrationExtent` identity and existing `ExtentMutation` FAST target-store support.
- Produces: `migrate_claimed_nv_extent_to_ssd(pool, identity, pages, fault) -> Result<Vec<Result<bool>>>`, which performs one logical tree lookup, one guarded entry validation, one 8-byte target publish, and batch credit release.
- Produces: whole-extent abort/requeue on ordinary stale identity; no per-page relocation fallback.

- [x] **Step 1: Add failing completion tests**

  Cover one lookup/one target persist for 32 pages, locator relocation after FAIR split, member generation mismatch, fragmented SSD targets, and before/after-target-store crash cuts.

- [x] **Step 2: Run focused ktests and confirm the new assertions fail on the per-page completion path**

- [x] **Step 3: Implement extent-granularity re-resolution and guarded publication**

  Under the existing `publication -> leaf` order, locate by `(fs_id, inode, logical_start)`, validate range/NV base/member generations/contiguous SSD targets once, and persist the aligned target word. Treat the candidate locator as a hint only.

- [x] **Step 4: Remove per-page validation and relocation fallback from normal completion**

  Completion groups by stable extent identity, calls the extent publisher once, batch-reclaims only after successful publication, and requeues the entire pinned extent on a normal stale result.

- [x] **Step 5: Run focused and complete rNVPCe integrity ktests**

  Expected: all optimistic overlap, FAST/FAIR recovery, contiguous extent, reservation-credit, and ABA tests pass.

### Task 4: Verify Build, Recovery, Concurrency, And Performance

**Files:**
- Create: `docs/experiments/reports/OPTIMISTIC-EXTENT-MIGRATION-2026-10-22.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/016-extent-migration-lease/README.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/README.md`

**Interfaces:**
- Consumes: the completed optimistic migration implementation and existing experiment scripts/configuration.
- Produces: reproducible functional, crash-consistency, latency, and fio evidence.

- [x] **Step 1: Run formatter, diff checks, release kernel build, and complete kernel ktests**

  Record exact commands, exit codes, and passed/failed counts. No success claim from a filtered suite alone.

- [ ] **Step 2: Run the targeted concurrency and crash matrix**

  Verify non-overlapping same-inode writes progress during migration, overlapping generations wait/retry outside locks, split relocation succeeds at extent granularity, stale completion requeues all pages, and every target-store fault cut recovers a readable file.

- [x] **Step 3: Run the established 2 GiB fio comparison**

  Use the existing 64 MiB dirty limit, 32 MiB low watermark, historical CPU isolation, real `/dev/nvme1n1p1`, and the retained baseline procedure. Measure 4 KiB random buffered writes and 128 KiB sequential buffered writes at 1 and 16 jobs, including drain/sync.

- [ ] **Step 4: Capture phase counters and verify the intended mechanism**

  Confirm one extent claim/leaf validation/target persistence per contiguous extent, no publication/range gate held across I/O, and no per-page leaf revalidation growth for 128 KiB requests.

- [x] **Step 5: Write the report and update plan status**

  Separate correctness evidence from performance results. Record regressions honestly; do not mark the design complete if fragmented-target or recovery cases remain unresolved.
