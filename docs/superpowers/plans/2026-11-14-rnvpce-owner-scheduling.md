# rNVPCe Owner Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce 32-thread foreground contention by preventing an active inode from being inserted into its owner worker's ready queue more than once.

**Architecture:** Replace the ready-inode Boolean with an `IDLE/QUEUED/RUNNING/RERUN` volatile state machine. Foreground publication only queues an idle inode or marks a running inode for another turn. Owner-directed post-commit wakeup was tested separately and rejected because it reduced worker parallelism; the final implementation preserves the existing coalesced all-worker notification policy.

**Tech Stack:** Rust kernel code, Asterinas ktests, QEMU fio on `/dev/nvme1n1p2`.

**Spec:** User-approved owner-directed scheduling and dirty-credit batching direction from the 2026-11-14 session.

## Global Constraints

- Do not change redirect persistence ordering or authority semantics.
- Do not lose dirty work when foreground publication races worker dequeue or turn completion.
- Keep all-worker wakeup for capacity, progress, sync, failure and shutdown paths.
- Keep inode ownership stable while dirty, in-flight, completion-pending or reclaim-pending work exists.
- Do not merge the page-index migration prototype into this branch.

## Review Focus

- Publication racing `RUNNING -> IDLE` must either set `RERUN` or enqueue from `IDLE`.
- A running inode must not appear twice in its worker ready queue.
- Budget exhaustion and an extending entry must schedule another worker turn.
- Owner release must reset scheduling state only after all active work disappears.
- Ordinary owner wakeups must not weaken capacity-pressure progress.

---

### Task 1: Inode Work State Machine

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index/ownership.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`

- [x] Add a failing test proving extension during a worker turn does not duplicate the inode in the ready queue.
- [x] Implement `IDLE/QUEUED/RUNNING/RERUN` transitions and finish each worker turn without lost work.
- [x] Run focused ownership ktests and ktest compile checks.

### Task 2: Owner-Directed Foreground Wakeup

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

- [x] Add a failing test proving an inode request advances only its owner epoch under pressure.
- [x] Test owner-only ordinary notifications while retaining global wakeups on pressure paths.
- [x] Reject and remove the owner-only candidate after real-device A/B showed reduced migration parallelism.

### Task 3: Verification and Performance

**Files:**
- Create: `docs/experiments/reports/RNVPCE-OWNER-SCHEDULING-2026-11-14.md`

- [x] Run formatting, normal and ktest compile checks, and focused QEMU ktests.
- [x] Run release 4/16/32-thread 4 KiB random-overwrite A/B tests on `/dev/nvme1n1p2`.
- [x] Record throughput, mean/p99 latency, correctness checks and remaining contention.
