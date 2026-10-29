# Dirty Migration Extent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rTIO's full dirty-page scan, global sort, and per-page claim loop with an incremental dirty migration extent index whose extents are structurally limited to 128 pages, then migrate each claimed range with batched coordination while blocking only overlapping foreground writes.

**Architecture:** Selected per-page descriptors remain the persistent authority. A partitioned DRAM index incrementally coalesces logically and physically contiguous dirty NVPages into extents of at most 128 pages. Candidate selection snapshots exact descriptor identities while installing one logical range lease, acquires one physical NV-slot range lease, batch-claims baseline generations, reserves an allocation group, and carries the resulting ticket through completion. Foreground writes query the claimed-range interval map and wait outside all resource locks.

**Tech Stack:** Rust `no_std`, Asterinas kernel, rNVPCe descriptors and page redirection, EXT4, virtio-blk, OSDK/QEMU ktests, fio diagnostics.

**Spec:** `docs/superpowers/specs/2026-10-29-dirty-migration-extents-design.md`

## Global Constraints

- `MAX_DIRTY_MIGRATION_EXTENT_PAGES` is exactly 128 and is enforced by every constructor, merge, recovery rebuild, and claim API.
- Persistent per-page descriptors remain authoritative until exact SSD publication and descriptor clearing complete.
- Candidate claim is DRAM-only; do not persist a per-page `NvWriteback` transition.
- No partition, inode, allocator, or baseline-generation lock may cross an I/O or wait boundary.
- The migration critical path must not route an inode extent tree or use reverse lookup.
- Existing unrelated changes in `docs/experiments/reports/NVPAGE-PARALLEL-RECLAIM-2026-10-28.md` must remain untouched and must not be included in feature commits.
- Use “index partition”, “dirty migration extent”, and “contiguous page range”; remove `shard` terminology from touched APIs and documents.

## Review Focus

- A 129th compatible page must create another indexed extent without dropping, duplicating, or reordering a page.
- Lease generation and exact descriptor handles must reject ABA without a persistent writeback state.
- A foreground waiter must release inode mutation and capacity locks before sleeping.
- Allocation-group ownership must be acquired before NVM-authoritative allocation metadata changes.
- Failure and crash paths must retain NV authority and requeue exactly once.

## File Structure

- Modify `kernel/src/fs/rnvpc/redirect_index.rs`: partitioned extent maps, ready/deferred queues, interval leases, local merge/split, and recovery rebuild.
- Modify `kernel/src/fs/rnvpc/baseline_generation.rs`: batch range claim and release.
- Modify `kernel/src/fs/rnvpc/frame/object_table.rs`: one DRAM physical-slot range lease for a contiguous migration extent; descriptor mutation and reuse paths consult it.
- Modify `kernel/src/fs/rnvpc/frame/redirect.rs`: range claim and exact-handle completion without persistent `NvWriteback` transitions.
- Modify `kernel/src/fs/rnvpc/persist.rs`: retain bounded descriptor publication/clear batches; remove claim-time state persistence.
- Modify `kernel/src/fs/rnvpc/mod.rs`: expose dirty-extent claim, conflict lookup, wait, requeue, and completion APIs.
- Modify `kernel/src/fs/fs_impls/ext4/migration.rs`: consume extent leases, reserve allocation groups, submit range BIOs, and carry tickets.
- Modify EXT4 allocator files located during Task 4: constrained allocation from an already owned block group and deferred wakeup.
- Modify `kernel/src/fs/fs_impls/ext4/inode/mod.rs`: prefix/overlap/suffix foreground write loop.
- Modify rNVPCe and EXT4 ktests in `kernel/src/fs/rnvpc/` and `kernel/src/fs/fs_impls/ext4/`.
- Add the final performance results to a new report under `docs/experiments/reports/`; keep raw data untracked under the experiment workspace.

### Task 1: Lock the 128-Page Extent Semantics with Failing Tests

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Add `pub(crate) const MAX_DIRTY_MIGRATION_EXTENT_PAGES: usize = 128`.
- Add `DirtyMigrationExtent::try_new(...) -> Result<Self>` and checked accessors.

- [ ] Add tests for one-page construction, 128-page construction, rejection of zero/129 pages, logical discontinuity, NV-slot discontinuity, inode boundary, and partition boundary.
- [ ] Add merge/split tests: 256 compatible pages become two 128-page extents; clearing page 64 creates `[0,64)` and `[65,128)`; reinsertion restores only compatible neighbors.
- [ ] Run `cargo osdk test rnvpce_dirty_migration_extent` and confirm the new tests fail before implementation.
- [ ] Implement only the checked extent value object and local merge/split helpers.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and `cargo osdk test rnvpce_dirty_migration_extent` again; commit the value object and tests.

### Task 2: Replace Full Scan and Global Sort with an Incremental Partitioned Index

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/recover.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Add `RedirectDirectory::publish_dirty(handle)`, `replace_dirty(old, new)`, and `clear_dirty(handle)` local-update APIs.
- Add `claim_ready_extents(fs_id, page_budget) -> Result<Vec<MigrationExtentLease>>`.
- Add `requeue_extent(lease)` and `rebuild_dirty_extents(iter)`.
- Rename touched `RedirectShard`/`shard_*` identifiers to `RedirectPartition`/`partition_*`.

- [ ] Add tests proving publication updates only neighboring extents, ready queues are round-robin, claimed extents are not selected twice, and page budgets never split an extent above its 128-page invariant.
- [ ] Add an instrumentation test asserting candidate selection does not enumerate every dirty page or invoke a global sort.
- [ ] Implement per-partition ordered extent maps and ready queues; keep exact redirect lookup behavior unchanged.
- [ ] Rebuild the same structures during recovery and compare recovery output with incremental construction for shuffled descriptors.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused index ktests; commit the incremental index.

### Task 3: Add Logical and Physical DRAM Range Leases

**Files:**
- Modify: `kernel/src/fs/rnvpc/baseline_generation.rs`
- Modify: `kernel/src/fs/rnvpc/frame/object_table.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/persist.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Test: `kernel/src/fs/rnvpc/persistent_tests.rs`

**Interfaces:**
- Add `BaselineGenerations::try_claim_range(keys) -> Result<BaselineRangeClaim>`.
- Add `ObjectTable::claim_migration_range(base_slot, len) -> Result<NvSlotRangeLease>`; this acquires one range-table lock, not every slot lock.
- Add `migration_overlap(fs_id, inode, range) -> Option<MigrationWaitToken>`.
- Replace page-oriented claim with `claim_dirty_extent(lease) -> Result<ClaimedMigrationExtent>`.

- [ ] Add failing tests for one logical/physical range claim, non-overlapping same-inode concurrency, exact logical or physical overlap exclusion, stale lease generation, descriptor ABA, and lock release on partial validation failure.
- [ ] Add a crash-boundary test showing candidate claim leaves persistent descriptors in `NvDirty` and recovery reselects them.
- [ ] Snapshot exact descriptor handles while installing the logical lease under one partition lock; add a separate physical-slot interval table and acquire one lease for the contiguous slots. Make replacement, clear, and slot reuse reject or wait on an overlapping lease.
- [ ] Implement the baseline range claim under one baseline-generation lock and use rollback guards for partial failure. Do not call the existing per-slot object lock once per migrated page during candidate claim.
- [ ] Remove claim-time `NvDirty -> NvWriteback` persistence; keep state changes required at final SSD publication/reclaim only.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused range-lease ktests; commit range leases and batch coordination.

### Task 4: Reserve Allocation Groups Before Target Mutation

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: EXT4 allocator implementation found by `rg -n "alloc_rnvpce|group.*migration|try_claim" kernel/src/fs/fs_impls/ext4`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Add an RAII `MigrationGroupReservation` acquired before target allocation.
- Add allocator entry point `alloc_rnvpce_blocks_in_group(reservation, count)` or the existing allocator's equivalent constrained form.
- Add deferred-extent wakeup keyed by released group capacity.

- [ ] Add tests that two migrations cannot mutate one group's allocation metadata concurrently, a busy group causes no descriptor transition, and release wakes a deferred extent without rediscovery.
- [ ] Refactor target preparation to choose and reserve a suitable group before NVM allocation metadata mutation.
- [ ] On exhaustion, move the exact extent ID to a deferred queue; on release, requeue it once.
- [ ] Verify rollback returns blocks and group ownership on every allocation/BIO submission failure.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused allocation-group ktests; commit group-aware target reservation.

### Task 5: Convert rTIO Flush to Extent Tickets

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Replace `rnvpce_redirect_ranges` plus per-page claim with `claim_ready_migration_extents(page_budget)`.
- Carry `MigrationExtentTicket { lease, descriptor_handles, baseline_claim, targets, bios }` through flush and completion.

- [ ] Add tests for one 128-page extent producing one claim/ticket, multiple extents filling a round budget, no duplicate selection, and exact requeue after BIO failure.
- [ ] Refactor `migrate_nv_round` so candidate timing covers queue pop and batch claim separately from target allocation and BIO submission.
- [ ] Remove reverse lookup and inode/root/leaf routing from the migration candidate and completion path.
- [ ] Preserve orderless submission and the existing single device-flush boundary for a completed migration batch.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused migration-ticket ktests; commit extent-ticket migration.

### Task 6: Split Foreground Writes Around Claimed Extents

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Add `first_migration_overlap(fs_id, inode, range) -> Option<MigrationConflict>`.
- Add `wait_for_migration(token)`, which may be called only after all filesystem/resource guards are dropped.

- [ ] Add tests for no conflict, conflict at start, maximal prefix before a middle conflict, retrying the suffix after wake, two sequential conflicts, and partial-write error semantics.
- [ ] Add a lock-order test/instrumentation assertion that no inode mutation, extent, allocator, or capacity guard is held at the wait call.
- [ ] Refactor `write_rnvpce_at` into a logical-order loop: query, commit prefix, drop guards, wait, re-query, continue.
- [ ] Remove the existing whole-request retry caused by any per-page `NvWriteback` member.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused foreground-overlap ktests; commit foreground range coordination.

### Task 7: Complete, Reclaim, and Recover by Exact Ticket Identity

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/recover.rs`
- Test: `kernel/src/fs/rnvpc/persistent_tests.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

- [ ] Add tests for stale completion, foreground replacement after lease release, baseline generation change, flush failure, publication failure, and recovery at every persistence boundary.
- [ ] Publish completed SSD baselines and descriptor state in bounded batches using exact ticket handles; never rediscover ownership by tree routing or reverse lookup.
- [ ] Integrate plan 017 parallel reclaim so completion marks exact pages reclaimable and foreground allocation may publish them to per-CPU free caches.
- [ ] Release the lease and notify its wait key at the protocol-defined final point; prove a late completion cannot clear a newer descriptor.
- [ ] Rebuild ready extents from persistent descriptors after simulated crash and verify repeated migration is idempotent.
- [ ] Run `cargo osdk check --ktests --package aster-kernel` and the focused completion/recovery ktests; commit completion and recovery.

### Task 8: Verify Correctness and Measure the Changed Bottleneck

**Files:**
- Modify: rNVPCe batch/latency metrics files found by `rg -n "nv_candidate_select|nv_flush_total" kernel/src`
- Create: `docs/experiments/reports/DIRTY-MIGRATION-EXTENTS-2026-10-29.md`

- [ ] Add counters for ready-queue pop, local rebuild, logical lease, physical-slot lease, baseline batch claim, group defer, target allocation, BIO submit, completion publication, and reclaim.
- [ ] Run `cargo osdk check --ktests --package aster-kernel`.
- [ ] Run the complete kernel ktest suite with `cargo osdk test` and record pass/fail totals.
- [ ] Run the existing 2 GiB fio experiments with 64 MiB dirty high watermark and 32 MiB low watermark: 4 KiB random write and 128 KiB sequential write on the available authorized NVMe test partition.
- [ ] Compare calls, total time, average time, pages per claim, duplicate selections, deferred-group wakeups, and foreground capacity wait against the latest 2026-10-29 diagnostic.
- [ ] Verify the steady-state selector performs no full dirty-page scan/global sort and that no indexed extent exceeds 128 pages.
- [ ] Record commands, commit, hardware/device, raw-data path, tables, and conclusions in the report; do not add raw CSV or generated plots to Git.
- [ ] Review `git diff`, run `git diff --check`, and commit only feature code/tests/spec/plan/report, excluding the pre-existing modified report.
