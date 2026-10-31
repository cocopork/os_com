# rNVPCe Range Write Transactions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Native rNVPCe metadata work proportional to redirect extents so one contiguous 128 KiB write is one range transaction instead of 32 page transactions.

**Architecture:** Add a range planner over the SSD extent tree and Native redirect entries, replace Native per-page redirect versioning with an entry interval map, and publish new data through a two-fence range transaction. Complete the conversion with range-owned runtime slot state while preserving plan 018's entry migration and reclaim protocol.

**Tech Stack:** Rust 2024, Asterinas kernel ktests, OSDK/QEMU, fio/QEMU with a real NVMe partition.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/019-range-write-transactions/README.md`

## Global Constraints

- `PageKey` is a lookup coordinate; `DirtyNvExtentEntry` is the Native data authority and lifecycle identity.
- A redirect entry contains equal-length contiguous logical, NV-slot, and SSD-block ranges and at most 128 pages.
- Never hold a spinlock across NVDIMM persistence, SSD I/O, device flush, sleep, or wait.
- New data and inactive records are durable before selectors; selectors are durable before inode reachability.
- Multi-page `write(2)` is not made failure-atomic, but every visible entry is complete and recoverable.
- Preserve rNVPC WAL and persistent metadata-object generation/descriptor behavior.
- Preserve unrelated dirty-worktree changes.

## Review Focus

- A write crossing an SSD physical discontinuity must publish separate valid entries without skipping or duplicating bytes.
- An unaligned overwrite may read only its partial boundary pages and must preserve untouched bytes.
- A writer racing entry migration must release resources, wait outside locks, and replan without an ABA slot lookup.
- A crash between multiple selector publications must leave only complete recoverable entries and no reachable uninitialized data.
- A request larger than 128 pages must complete through an internal loop rather than return a limit-induced short write.

---

### Task 1: Range Planner and Range Lookup APIs

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/object_table.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produces: `RangeWritePlan`, `WriteExtent`, `RedirectDirectory::lookup_page()`, `RedirectDirectory::lookup_overlaps()`, and the minimum range-owner staging/mutation interfaces from the spec.
- Consumes: existing SSD extent routing, `DirtyNvExtentEntry`, and `NvRedirectExtentRecord`.

- [ ] **Step 1: Write failing range-planner ktests**

Add one aggregate `rnvpce_range_write_plan` test covering a contiguous 32-page SSD extent, an SSD physical discontinuity, a hole/unwritten boundary, old EOF, an existing dirty-entry boundary, an unaligned two-boundary-page write, and a 129-page request. Assert exact `WriteExtent` ranges including first/last page offsets and that the 32-page case performs one SSD tree range route and one redirect overlap lookup.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cd kernel && cargo osdk test rnvpce_range_write_plan --qemu-args="-enable-kvm -cpu host -display none"`

Expected: compile/test failure because the range planner and overlap API are absent.

- [ ] **Step 3: Implement range lookup and planning**

Define `RangeWritePlan` and `WriteExtent` with the signatures in the spec. Replace page-index partition selection with `(fs_id, inode) -> Arc<InodeRedirectMap>` and add predecessor-plus-forward interval lookup. Add `lookup_tree_extent_ranges()` to `write_transaction.rs`; it walks leaf entries and siblings from the first routed leaf and emits maximal SSD/hole/unwritten runs without rerouting each page. Keep compatibility page lookup as a wrapper around range lookup during migration.

- [ ] **Step 4: Add the minimum range-owner staging API**

In `frame/object_table.rs` and `frame/redirect.rs`, implement `begin_reserved_data_ranges()`, `begin_range_data_mutation()`, `install_data_range_owner()`, `remove_data_range_owner()`, and `data_range_segments()` with a range owner that may initially retain per-slot backing records internally. This task establishes entry identity before Task 2 removes per-page handles; Task 3 removes the remaining per-slot update cost.

- [ ] **Step 5: Run the focused test and verify GREEN**

Run the Step 2 command.

Expected: all planner scenarios pass and the contiguous 32-page counters equal one range route and one overlap lookup.

- [ ] **Step 6: Commit Task 1**

```bash
git add kernel/src/fs/rnvpc/write_transaction.rs kernel/src/fs/rnvpc/redirect_index.rs \
  kernel/src/fs/rnvpc/frame/object_table.rs kernel/src/fs/rnvpc/frame/redirect.rs \
  kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
git commit -m "Add rNVPCe range write planning"
```

### Task 2: Entry-level Native Redirect Publication

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/persist.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: Task 1 `RangeWritePlan`, `WriteExtent`, and overlap lookup.
- Produces: `RedirectDirectory::publish_entries()`, `RedirectDirectory::remove_entry()`, and `Rnvpc::publish_redirect_entries()`; Native writes no longer reserve per-page logical versions.

- [ ] **Step 1: Write failing entry-publication ktests**

Extend `rnvpce_range_write_plan` with assertions that a contiguous 128 KiB overwrite publishes one record and one DRAM entry, an internal 4 KiB overwrite of an existing dirty entry preserves the same `Arc` and selector, discontinuities publish exactly one entry per range, copy faults publish only complete initialized prefixes, and no Native per-page pending/committed handle is created.

- [ ] **Step 2: Add selector-boundary crash tests**

Add `FaultSpec { point, occurrence }` to the test-only fault interface. Add scenarios before the data fence, after the data fence, after selector occurrence 1 through N, after the selector fence, and between selector and inode-generation publication. Recover each image and assert that every visible entry has initialized data, records newer than the authoritative inode generation are ignored, and unreachable staging ranges are reclaimable.

- [ ] **Step 3: Run tests and verify RED**

Run the Task 1 focused command.

Expected: new entry-identity/count and crash assertions fail against per-page reservation/publication.

- [ ] **Step 4: Implement two-fence range publication**

Change `publish_redirect_batch()` into `publish_redirect_entries()` over prepared ranges. Before persistence, preallocate every entry, inode-map node, and publication container and validate all overlaps in canonical lock order. Persist all new data ranges and inactive records, fence once, publish every head selector, then fence once. Install the preallocated entries with an infallible no-allocation transition; an impossible post-selector violation latches `publication_uncertain`. Replace Native `reserve_versions()/commit_reserved()` calls with entry validation and publication. Existing NV overwrite persists bytes without selector or entry replacement.

- [ ] **Step 5: Route EXT4 foreground writes through the plan**

Replace the three page-mapping passes in `write_rnvpce_at()` and `range_write_new_pages()` with one plan. Reserve capacity outside locks, acquire entry writer references in logical order, validate one inode generation and entry identities, and retry after migration outside locks. Bump the redirect image version, store `visibility_generation` in each record, split at old EOF, and publish inode state only for root/size/block-count changes. Recovery ignores records newer than the authoritative inode generation.

- [ ] **Step 6: Verify focused and rNVPCe regressions**

Run the Task 1 command and each existing aggregate rNVPCe integrity/recovery entry point by exact test name.

Expected: all selected tests pass; the contiguous 128 KiB scenario reports one entry publication and no per-page Native descriptor transaction.

- [ ] **Step 7: Commit Task 2**

```bash
git add kernel/src/fs/rnvpc/redirect_index.rs kernel/src/fs/rnvpc/frame/redirect.rs \
  kernel/src/fs/rnvpc/persist.rs kernel/src/fs/rnvpc/write_transaction.rs \
  kernel/src/fs/fs_impls/ext4/inode/mod.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
git commit -m "Publish rNVPCe writes as range transactions"
```

### Task 3: Range-owned Native Slot State

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame/object_table.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/frame/migration.rs`
- Modify: `kernel/src/fs/rnvpc/slot_allocator.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: Task 2 entry identity and publication APIs.
- Produces: range allocation/claim/reclaim APIs whose lock acquisitions are proportional to entries, while compatibility metadata/WAL slot APIs remain per-slot.

- [ ] **Step 1: Write failing range-owner ktests**

Add one aggregate `rnvpce_range_owned_slots` test covering 32-page allocate/publish, concurrent reader versus reclaim, migration claim versus writer, complete range free/reuse, stale `Arc` work after reuse, fragmented allocator fallback, and recovery reconstruction. Assert one range-state lock acquisition for allocate/claim/reclaim in the contiguous case.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cd kernel && cargo osdk test rnvpce_range_owned_slots --qemu-args="-enable-kvm -cpu host -display none"`

Expected: lock-count assertions fail because `ObjectTable` updates each slot.

- [ ] **Step 3: Implement Native range ownership**

Complete the Task 1 range-owner conversion by replacing its internal per-slot backing updates with `DataSlotRange` state keyed by start slot. Make a `DirtyNvExtentEntry` reader/writer guard pin the range owner. Allocation initializes private pages before installing the owner; migration claims the entry once; reclaim clears the selector, waits readers, removes one range owner, and returns one allocator range. Retain per-slot records only for metadata, WAL compatibility, and test-only legacy interfaces.

- [ ] **Step 4: Verify range-owner and recovery regressions**

Run the Step 2 command, the exact existing allocator/domain tests, and the rNVPCe recovery aggregate.

Expected: all pass with no overlapping owner, stale reclaim, capacity leak, or per-page Native reclaim lock count.

- [ ] **Step 5: Commit Task 3**

```bash
git add kernel/src/fs/rnvpc/frame/object_table.rs kernel/src/fs/rnvpc/frame.rs \
  kernel/src/fs/rnvpc/frame/redirect.rs kernel/src/fs/rnvpc/frame/migration.rs \
  kernel/src/fs/rnvpc/slot_allocator.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
git commit -m "Track rNVPCe data slots by range"
```

### Task 4: Compatibility Cleanup and Validation

**Files:**
- Modify: `kernel/src/fs/rnvpc/index.rs`
- Modify: `kernel/src/fs/rnvpc/descriptor.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Create: `docs/experiments/reports/RNVPCE-RANGE-WRITE-TRANSACTIONS-2026-10-31.md`
- Create: `docs/experiments/scripts/rnvpce-range-write-20261031/`

**Interfaces:**
- Consumes: Tasks 1--3 range transaction and range-owner APIs.
- Produces: Native production code without per-page `DescriptorHandle`, logical-version, or allocation-sequence dependencies, plus reproducible correctness/performance evidence.

- [ ] **Step 1: Remove Native per-page compatibility dependencies**

Delete or confine to rNVPC/ktest the Native data uses of `PageEntry`, `DescriptorHandle`, per-page `logical_version`, and data-page `allocation_seq`. Keep persistent metadata-object references and rNVPC WAL behavior unchanged.

- [ ] **Step 2: Verify request-size and mode isolation**

Add tests that a 1024 KiB write loops over at most 128-page entries and returns the full byte count, and that rNVPC WAL still uses its descriptor/generation path. Run both tests by exact name.

Expected: both pass without a limit-induced short write or rNVPC behavior change.

- [ ] **Step 3: Run compile and kernel regression gates**

Run: `cd kernel && cargo osdk check --ktests --package aster-kernel`

Expected: exit 0.

Run these exact existing regressions in addition to the new aggregate tests:

```text
rnvpce_dirty_nv_entry_foreground
rnvpce_nv_entry_claim
rnvpce_nv_entry_work_item
rnvpce_data_slot_recovery
rnvpce_domain_allocator
rnvpce_redirect_publish_survives_every_selector_boundary
rnvpce_old_image_version_is_rejected
persistent_wal_root_split_faults_recover_from_committed_records
persistent_wal_and_soft_update_owners_recover_in_shared_pool
```

Expected: all selected tests pass.

- [ ] **Step 4: Run real-device structural/performance validation**

Use the existing 2 GiB, 128 MiB pool, 64 MiB data-limit setup. Run three alternating rounds of 128 KiB sequential overwrite at jobs 1 and 16 on `/dev/nvme1n1p1`, falling back to the previously authorized `/dev/nvme1n1p2` only if p1 is occupied. Record release bandwidth and a separate sampled diagnostic run with range-route, planned-entry, entry-lock, fence, capacity-wait, rTIO candidate/BIO, and queue-depth counters.

Expected: every fio transfer completes, sync/unmount succeeds, `e2fsck -fn` passes, and the diagnostic confirms metadata operations scale with entry count. Compare release medians with 520.19 MiB/s at jobs 1 and 288.25 MiB/s at jobs 16; report measured results without imposing a required speedup.

- [ ] **Step 5: Record results and commit**

Write exact commit/image hashes, commands, configuration, correctness results, all bandwidth samples/medians, structural counts, and remaining bottlenecks. Track scripts and the report, not raw images, CSV files, or plots.

```bash
git add kernel/src/fs/rnvpc/index.rs kernel/src/fs/rnvpc/descriptor.rs \
  kernel/src/fs/rnvpc/redirect_index.rs kernel/src/fs/rnvpc/write_transaction.rs kernel/src/fs/rnvpc/frame.rs \
  kernel/src/fs/rnvpc/frame/object_table.rs kernel/src/fs/rnvpc/frame/redirect.rs kernel/src/fs/rnvpc/persist.rs \
  kernel/src/fs/fs_impls/ext4/inode/mod.rs \
  kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs \
  docs/experiments/reports/RNVPCE-RANGE-WRITE-TRANSACTIONS-2026-10-31.md \
  docs/experiments/scripts/rnvpce-range-write-20261031
git commit -m "Validate rNVPCe range write transactions"
```
