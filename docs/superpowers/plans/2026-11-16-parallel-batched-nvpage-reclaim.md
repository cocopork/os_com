# Parallel Batched NVPage Reclaim Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the single reclaim-queue and per-entry cleanup bottlenecks that prevent rNVPCe 4 KiB random overwrite from scaling to 32 foreground threads.

**Architecture:** Durable results remain owned by their submitting rTIO worker and enter one reclaim queue per worker. Each worker drains its own queue in bounded batches; native entries share persistence barriers and allocation-domain cleanup, while legacy entries retain their compatibility path. The DRAM data-range owner index uses the same allocation-domain partitioning as the slot allocator.

**Tech Stack:** Rust kernel code, Asterinas ktests, Docker/OSDK, QEMU fio on `/dev/nvme1n1p2`.

**Spec:** [`docs/os_com_implementation/os_com_refactor_plan/024-parallel-batched-nvpage-reclaim/README.md`](../../os_com_implementation/os_com_refactor_plan/024-parallel-batched-nvpage-reclaim/README.md)

## Global Constraints

- Preserve the orderless migration authority protocol and on-NVM redirect entry format.
- Never split one redirect extent entry across reclaim executors.
- Do not reintroduce per-page allocation generation or per-page tree validation.
- Do not hold allocation-domain or redirect-index spinlocks across persistence operations.
- Foreground capacity waiters must not compete for a global reclaim queue.
- Keep legacy visibility-generation-zero reclaim behavior correct.
- Preserve all pre-existing uncommitted work in this worktree; commits are deferred unless explicitly requested.

## Review Focus

- A completion delivered to worker 3 must be invisible to workers 0-2 and still count toward filesystem-wide ready pages.
- A worker with a partial final batch must reclaim it without waiting for 1024 pages.
- An error after pending-free publication must not free a slot whose selector remains live.
- Two batches operating on different allocation domains must not acquire one common data-range lock.
- Sync and shutdown must drain all worker queues even when foreground writers are absent.

---

### Task 1: Per-Worker Reclaim Ownership

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration/reclaim.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`

**Interfaces:**
- Produces: `ReclaimQueueSet<T>::new(worker_count)`, `publish(worker_id, value, units)`, `claim(worker_id)`, `worker_ready_units(worker_id)`, and `total_ready_units()`.
- Consumes: `MigrationBatch::owner_worker_id()` and the configured rTIO worker count.

- [x] **Step 1: Add failing queue-set tests** proving worker isolation, exact total accounting, and invalid-worker rejection.
- [x] **Step 2: Run focused ktests** and require failure because `ReclaimQueueSet` does not exist.
- [x] **Step 3: Implement the queue set and route durable completion to its owner worker.**
- [x] **Step 4: Move reclaim draining into each rTIO worker loop; foreground capacity wait only wakes workers and waits.**
- [x] **Step 5: Run focused migration/reclaim ktests and ktest compile check.**

### Task 2: Allocation-Domain Data-Range Index

**Files:**
- Modify: `kernel/src/fs/rnvpc/frame/object_table.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/rnvpc/frame/object_table.rs`

**Interfaces:**
- Produces: domain-local owner-map operations and grouped `remove_data_range_owners`.
- Consumes: the slot allocator's domain geometry and the invariant that native data ranges never cross a domain.

- [x] **Step 1: Add failing tests** for independent-domain mutation and cross-domain rejection.
- [x] **Step 2: Run focused ktests** and verify the new grouped interface is absent.
- [x] **Step 3: Partition `data_ranges` by allocation domain and implement grouped removal.**
- [x] **Step 4: Run object-table, slot-allocator, and redirect integrity tests.**

### Task 3: Native Batch Reclaim

**Files:**
- Modify: `kernel/src/fs/rnvpc/persist.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produces: `Rnvpc::reclaim_nv_entries(&[NvExtentEntryLease]) -> Result<usize>` and batched persistence helpers.
- Consumes: Task 2 grouped owner removal and allocator-domain grouped free operations.

- [x] **Step 1: Add failing tests** for one barrier per authority phase, batch capacity release, legacy fallback, and a fault between authority phases.
- [x] **Step 2: Run focused integrity tests** and verify failure before implementation.
- [x] **Step 3: Implement two-phase native batch persistence and grouped runtime cleanup.**
- [x] **Step 4: Keep `reclaim_nv_entry` as a one-entry wrapper and preserve legacy fallback.**
- [x] **Step 5: Run reclaim, recovery, and crash-consistency tests.**

### Task 4: Verification And Performance Matrix

**Files:**
- Create: `docs/experiments/reports/RNVPCE-PARALLEL-BATCHED-RECLAIM-2026-11-16.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/024-parallel-batched-nvpage-reclaim/README.md`

**Interfaces:**
- Consumes: Tasks 1-3 and the existing overwrite sweep/profiling harness.
- Produces: correctness evidence, repeated throughput results, and per-worker reclaim diagnostics.

- [x] **Step 1: Run format, diff, normal compile, ktest compile, and focused ktests.**
- [x] **Step 2: Build a release image and run smoke, sync, unmount, and e2fsck checks on `/dev/nvme1n1p2`.**
- [x] **Step 3: Run repeated 4/16/24/32-thread 4 KiB random-overwrite tests with 2 GiB files, 64 MiB data capacity, four workers, and four virtqueues.**
- [x] **Step 4: Record throughput plus per-worker ready/reclaimed pages, batch size, capacity wait, persistence count, and wake count; state regressions plainly.**

