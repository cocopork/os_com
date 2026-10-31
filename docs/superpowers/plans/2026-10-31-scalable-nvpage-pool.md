# Scalable NVPage Pool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the pool-wide NVPage free-list and capacity lock with fixed allocation domains and atomic accounting, then remove duplicate `RnvpcExtend` data-index serialization and verify correctness and scalability.

**Architecture:** Data and metadata slots use one fixed-domain physical allocator with an independent free-range index and lock per domain. Data, metadata, and DRAM-only WAL use an atomic capacity ledger. `RnvpcExtend` redirect APIs stop duplicating per-page state in the legacy pool index; per-page access pins remain intact.

**Tech Stack:** Rust 2024, Asterinas kernel ktests, OSDK/QEMU, fio/QEMU real-device experiment scripts.

**Spec:** `docs/superpowers/specs/2026-10-31-scalable-nvpage-pool-design.md`

## Global Constraints

- Never hold a spinlock across NVDIMM persistence, SSD I/O, device flush, sleep, or wait.
- Data and metadata share physical slots; WAL charges capacity but never receives a slot.
- A redirect entry never spans two allocation domains.
- Per-page `pins`, `persisting`, `key`, version, and access state remain authoritative for access concurrency.
- CPU-local free-range caches are excluded unless post-domain profiling still shows material domain-lock wait.
- Preserve unrelated dirty-worktree changes.

## Review Focus

- Concurrent capacity reservations must neither over-admit nor leak charges when physical allocation fails.
- Recovery must rebuild exactly one physical owner for every live Data/Metadata slot and set WAL usage to zero.
- Fragmentation and domain boundaries must return multiple valid contiguous ranges without duplicate slots.
- Metadata emergency repair frames must remain available when ordinary free slots are exhausted.
- `Rnvpc` WAL/legacy lookups must retain required `PoolState::index` behavior while `RnvpcExtend` avoids it.

---

### Task 1: Fixed-domain Free-range Allocator

**Files:**
- Create: `kernel/src/fs/rnvpc/slot_allocator.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`

**Interfaces:**
- Produces: `NvSlotAllocator::new(total_pages)`, `allocate_contiguous(pages, hint)`, `allocate_ranges(pages, hint)`, `free_range(range)`, `mark_allocated(range)`, and allocator statistics used by Task 2.
- Consumes: no new interfaces.

- [x] **Step 1: Write failing ktests**

Add one aggregate ktest named `rnvpce_slot_allocator` with real scenarios for contiguous reuse, cross-domain fallback, fragmented splitting, and model stress. Assert literal slot ranges, uniqueness, exact free counts, and rejection of double free/overlapping recovery marks.

- [x] **Step 2: Verify RED**

Run: `cd kernel && cargo osdk test rnvpce_slot_allocator --qemu-args="-enable-kvm -cpu host -display none"`

Expected: compile/test failure because `NvSlotAllocator` is absent.

- [x] **Step 3: Implement the domain allocator**

Use fixed domains with a minimum target size of 1024 pages, per-domain bitmap/segment summaries, atomic free/longest hints, and an authoritative no-summary fallback. Never hold two domain locks. Split only when one domain cannot satisfy the complete request.

- [x] **Step 4: Verify GREEN**

Run the Task 1 command again.

Expected: the aggregate test and all its scenarios pass.

### Task 2: Atomic Capacity Ledger and Pool Integration

**Files:**
- Modify: `kernel/src/fs/rnvpc/capacity.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/tests.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: Task 1 `NvSlotAllocator` allocation/free/recovery APIs.
- Produces: lock-free class reservation/release/statistics APIs and pool allocation paths without `Rnvpc::allocation`.

- [x] **Step 1: Write failing pool ktests**

Add one aggregate `rnvpce_domain_allocator` ktest with scenarios for capacity classes, parallel unique ranges, recovery free-range reconstruction, and WAL charges without slot consumption. Extend the existing parallel reclaim scenario to allocate multi-page ranges concurrently.

- [x] **Step 2: Verify RED**

Run: `cd kernel && cargo osdk test rnvpce_domain_allocator --qemu-args="-enable-kvm -cpu host -display none"`

Expected: tests fail because pool allocation still uses the global free-list/does not expose the tested domain behavior.

- [x] **Step 3: Replace global allocation state**

Convert capacity to atomic reservations, convert `next_alloc_seq` to checked atomic interval reservation, route Data/Metadata/recovery/rollback/free through `NvSlotAllocator`, and keep WAL capacity-only. Preserve repair-workspace drain and wakeup semantics. Slot initialization/free occurs after releasing the domain lock.

- [x] **Step 4: Verify GREEN and regression set**

Run: `cd kernel && cargo osdk test rnvpce_domain_allocator --qemu-args="-enable-kvm -cpu host -display none"`

Expected: focused tests pass.

Run the exact existing allocation/reclaim test names listed in the task report; do not use a prefix as an OSDK filter.

Expected: existing allocation/reclaim tests pass.

Run: `cd kernel && cargo osdk check --ktests --package aster-kernel`

Expected: exit 0.

### Task 3: `RnvpcExtend` Global-index Isolation

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/frame/migration.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: Task 2 slot allocator and atomic capacity paths.
- Produces: explicit redirect state transition/publication/reclaim APIs that do not update `PoolState::index`.

- [x] **Step 1: Write failing mode-isolation ktests**

Add one aggregate `rnvpce_pool_control_isolation` ktest whose scenarios hold the legacy pool-control lock while `RnvpcExtend` redirect publication, transition, migration-target assignment, and reclaim complete. Add a scenario proving the generic transition still updates its index.

- [x] **Step 2: Verify RED**

Run: `cd kernel && cargo osdk test rnvpce_pool_control_isolation --qemu-args="-enable-kvm -cpu host -display none"`

Expected: the redirect hot path blocks or the new explicit API is absent.

- [x] **Step 3: Add explicit redirect APIs**

Move `RnvpcExtend` call sites to explicit redirect transition/publication/reclaim methods. Remove their duplicate per-page legacy index updates only after auditing `transition_data_page()`, `publish_redirect_batch()`, `assign_migration_targets()`, and `reclaim_nv_entry()`. Retain generic legacy behavior.

- [x] **Step 4: Verify GREEN and rNVPCe regressions**

Run: `cd kernel && cargo osdk test rnvpce_pool_control_isolation --qemu-args="-enable-kvm -cpu host -display none"`

Expected: isolation tests pass.

Run the repository's rNVPCe aggregate test entry points by exact suffix; do not use `rnvpce_` as a prefix filter.

Expected: all rNVPCe tests pass.

### Task 4: Build, Recovery, and Performance Validation

**Files:**
- Create: `docs/experiments/reports/SCALABLE-NVPAGE-POOL-2026-10-31.md`
- Create or modify: task-specific scripts under `docs/experiments/scripts/`
- Modify: `docs/superpowers/specs/2026-10-31-scalable-nvpage-pool-design.md` only for measured conclusions.

**Interfaces:**
- Consumes: Tasks 1--3 implementation and counters.
- Produces: reproducible correctness and 1/16-job performance evidence.

- [x] **Step 1: Run compile and focused/full kernel tests**

Run: `cd kernel && cargo osdk check --ktests --package aster-kernel`

Expected: exit 0.

Run all exact rNVPCe aggregate test entry points used by Tasks 1--3 and the existing recovery aggregate.

Expected: all selected tests pass.

- [x] **Step 2: Run crash/recovery regression**

Run the existing rNVPCe recovery aggregate and verify no overlapping slot ownership, leaked capacity assertion, or recovery error.

Expected: all recovery tests pass.

- [x] **Step 3: Run real-device performance comparison**

Use the existing 2 GiB workload and 128 MiB pool with the 64 MiB data limit. Run three alternating rounds of 128 KiB sequential overwrite at jobs 1 and 16, plus the allocation/reclaim pipeline breakdown. Use `/dev/nvme1n1p1`, falling back to the previously authorized `/dev/nvme1n1p2` only when p1 is occupied.

Expected: every fio point, sync, unmount, and `e2fsck -fn` succeeds. Report medians and lock wait/hold measurements; compare against 434.91 MiB/s (j1) and 244.86 MiB/s (j16) pre-change medians.

- [x] **Step 4: Decide CPU-local cache phase from evidence**

If allocation-domain lock wait remains a material foreground component, implement a separate TDD task for the bounded 16/32-page CPU-local cache. Otherwise record that the cache is not justified and leave it unimplemented.

- [x] **Step 5: Record results**

Write commands, exact revisions/configuration, correctness results, bandwidth samples/medians, and the cache decision into the report. Do not track raw images, CSV files, or plots.
