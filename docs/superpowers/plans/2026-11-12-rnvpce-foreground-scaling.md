# rNVPCe Foreground Scaling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce 4 KiB overwrite contention in redirect-extension accounting and post-commit migration notification without changing rNVPCe persistence or migration semantics.

**Architecture:** Keep persistent redirect entry publication unchanged. Move the exact queued-page and ready-queue bookkeeping out of the per-inode migration spinlock into per-inode atomics, while retaining the locked ownership and phase counters. Preserve all-worker migration notification semantics, but make each worker notification edge-triggered so repeated post-commit requests coalesce until the worker acknowledges the edge.

**Tech Stack:** Rust kernel code, Asterinas ktests, QEMU fio on `/dev/nvme1n1p2`.

**Spec:** User-approved optimization order in the 2026-11-12 session; EST route-cache work is explicitly deferred.

## Global Constraints

- Preserve NVM-authoritative redirect publication and existing FAST/FAIR persistence ordering.
- Preserve exact queued, in-flight, completion-pending and reclaim-pending page accounting.
- Preserve one stable owner worker while an inode has active migration state.
- No blocking, I/O or allocation while holding a spinlock.
- Do not modify the write-plan/EST route lookup in this change.

## Review Focus

- Extension racing with a worker dequeue must leave the inode queued or visible to that worker.
- A returned claim must restore both exact queued pages and ready-queue visibility.
- Owner release must observe all atomic and locked page counters at zero.
- A notification racing with worker sleep must not be lost.
- Capacity/progress pressure must still wake every worker that may own dirty work.

---

### Task 1: Lockless Common-Path Extension Accounting

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index/ownership.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`

**Interfaces:**
- Consumes: `finish_entry_extension`, `MigrationWorkerSet::extend_dirty`, `claim_pages`, `return_claim`.
- Produces: exact per-inode atomic queued-page accounting and edge-triggered ready-inode enqueue.

- [x] Add a ktest that publishes one entry, extends it repeatedly, claims it once, and verifies exact queued/in-flight accounting and a single ready-inode dequeue.
- [x] Run the focused ktest and verify it fails against the old per-extension scheduling behavior assertion.
- [x] Move queued-page and ready-queued state to per-inode atomics; retain ownership, generation and migration phase counters under the existing lock.
- [x] Make extension accounting use one per-inode page addition, one per-worker page addition, and an edge-triggered ready enqueue without acquiring the inode migration lock on the common path.
- [x] Re-run the focused test and the ktest compile check.
- [x] Run the 4/16/32-thread fio matrix and record the delta against the current clean baseline.

### Task 2: Edge-Triggered Migration Notification Coalescing

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: per-worker epoch and trigger arrays.
- Produces: `request_nv_migration_worker(worker_id)` with a per-worker pending bit and lost-wakeup-safe worker acknowledgement.

- [x] Add a ktest proving two notifications coalesce before acknowledgement, affect only the selected worker epoch, and a post-ack notification advances it again.
- [x] Run the focused ktest and verify the original unconditional epoch advancement fails it.
- [x] Add one pending bit per migration worker while preserving all-worker notification for post-commit migration requests.
- [x] Sample the worker epoch before acknowledging the pending edge so a concurrent notification cannot be lost.
- [x] Keep global failure, shutdown, capacity and progress paths able to notify all workers.
- [x] Re-run the focused notification test and the ktest compile check.
- [x] Run the same 4/16/32-thread fio matrix and compare both individual and combined effects.

### Task 3: Final Regression and Report

**Files:**
- Modify: `docs/experiments/reports/RNVPCE-AUTHORITY-HOTPATH-2026-11-11.md`

- [x] Run formatting, diff checks, focused ktests and the relevant real-device rNVPCe route.
- [x] Run at least three release-mode repetitions for any point whose change is within normal run-to-run variance.
- [x] Retain only changes that improve or preserve uninstrumented performance without reducing migration progress.
- [x] Record implementation, correctness evidence, raw result paths and unresolved write-plan/EST work.
