# rNVPCe Worker-Local Batching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve inode-owner locality while forming larger rTIO migration batches, and allow safe whole-inode load redistribution only when NV capacity pressure requires it.

**Architecture:** Normal migration uses each worker's own queued dirty-page count as its activation watermark and wakes only the inode owner. Capacity or explicit progress pressure wakes all workers and permits an idle worker to take a queued inode from an overloaded worker only when that inode has no in-flight, completion-pending, or reclaim-pending work. Device durability remains global, but sub-threshold ready batches no longer keep every worker runnable.

**Tech Stack:** Rust kernel code, Asterinas ktests, Docker/QEMU fio on `/dev/nvme1n1p2`.

**Spec:** [`docs/os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers/README.md`](../../os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers/README.md), extended by the approved 2026-11-15 worker-local batching and pressure-rebalancing design.

## Global Constraints

- Keep one execution owner per inode; two workers must never claim redirect entries from the same inode concurrently.
- Do not transfer an inode while it has in-flight, completion-pending, reclaim-pending, or running migration work.
- Preserve the existing orderless persistence protocol, NV authority rules, and per-batch completion ownership.
- Normal foreground publication must not broadcast to workers that do not own the inode.
- Capacity waiters, explicit progress waiters, sync, shutdown, and failure must retain bounded forward progress.
- Keep the existing 1024-page migration batch limit; change activation policy, not BIO size or persistent metadata format.

## Review Focus

- A worker with 1023 local dirty pages and no pressure remains parked even when other workers make the global total exceed 1024.
- A capacity waiter overrides the local watermark and wakes enough workers to make progress.
- Pressure rebalancing transfers only a queued, non-running inode with no in-flight/completion/reclaim state.
- A stale ready-queue item left after ownership transfer is discarded without losing the inode's new queue item.
- A sub-threshold durability tail does not spin workers, while threshold crossing and an idle tail still advance durability.

---

### Task 1: Worker-Local Activation And Owner Wakeup

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `Rnvpc::migration_worker_load_snapshot(fs_id, worker_id)` and `Rnvpc::nv_entry_owner_worker(fs_id, inode)`.
- Produces: `should_run_background_migration(worker_dirty_pages, capacity_waiters, progress_waiters) -> bool` and owner-local foreground wakeup behavior.

- [x] **Step 1: Add failing activation-policy tests**

  Pin that 1023 local pages remain parked without pressure, 1024 local pages run, and either waiter count overrides the watermark. Update the below-global-watermark foreground test so ordinary publication remains resident until an explicit drain/pressure event.

- [x] **Step 2: Run the focused tests and verify the old global-total policy fails them**

  Run the relevant `migration` and `nv_tests` ktests through the existing OSDK ktest runner; expect the local-watermark assertion to fail before implementation.

- [x] **Step 3: Implement worker-local activation and owner-only ordinary wakeup**

  Remove `total_dirty_pages` from normal activation. `request_nv_migration_for_inode()` reads the inode owner and that worker's queued pages, waking only the owner at 1024 pages; capacity/progress, sync, shutdown, and failure continue to use the global wake path.

- [x] **Step 4: Run focused ktests and ktest compile checks**

  Run the activation/wakeup tests and `cargo osdk check --ktests --package aster-kernel`; expect success.

### Task 2: Pressure-Only Whole-Inode Rebalancing

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index/ownership.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`

**Interfaces:**
- Consumes: worker ready-inode queues, `InodeMigrationState`, and the `IDLE/QUEUED/RUNNING/RERUN` work-state machine.
- Produces: `MigrationWorkerSet::rebalance_queued_inode(helper_id: usize) -> Result<bool>` and `Rnvpc::rebalance_migration_inode(fs_id: u64, helper_id: usize) -> Result<bool>`.

- [x] **Step 1: Add failing ownership-transfer tests**

  Test a successful queued-inode transfer, refusal for `RUNNING`, refusal with any in-flight/completion/reclaim pages, correct per-worker dirty/owned counters, and harmless discard of the old worker's stale queue node.

- [x] **Step 2: Run the ownership tests and verify the rebalancing interface is absent**

  Run the focused redirect-index ktests; expect compile failure or missing-method failure before implementation.

- [x] **Step 3: Implement atomic whole-inode transfer**

  Select the most-loaded victim, remove one queued inode, validate the safety predicates under its migration lock, update owner generation and worker counters, then enqueue it once on the helper. On validation failure, restore the victim queue without changing ownership.

- [x] **Step 4: Invoke rebalancing only under capacity/progress pressure**

  An idle pressured worker attempts one safe transfer before concluding it has no local work. Normal watermark operation never steals and never inspects another worker's ready queue.

- [x] **Step 5: Run focused ownership, migration, and compile checks**

  Expect all new race/counter tests and `cargo osdk check --ktests --package aster-kernel` to pass.

### Task 3: Edge-Driven Durability Progress

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/worker.rs`
- Test: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`

**Interfaces:**
- Consumes: global `ready_pages`, `pending_data_pages`, durability generation, and worker triggers.
- Produces: a wait predicate that wakes for a new completion edge, a full 1024-page epoch, an idle tail, or an explicit drain, but does not remain permanently true merely because one sub-threshold batch exists.

- [x] **Step 1: Add failing durability wake tests**

  Pin that a ready tail below 1024 does not keep a worker runnable after its completion edge is consumed, that crossing 1024 advances once, and that queued/pending work prevents premature idle-tail flush.

- [x] **Step 2: Run the focused durability tests and observe the sticky-ready behavior**

  Expect the sub-threshold non-spinning assertion to fail before implementation.

- [x] **Step 3: Remove sticky `ready_batch_count != 0` wake behavior**

  Use generation changes and explicit threshold/idle conditions as edges. Keep exactly one global durability leader and route durable results to the submitting batch owner.

- [x] **Step 4: Run focused durability and migration tests**

  Expect the new wake tests, existing durability accounting tests, and ktest compile check to pass.

### Task 4: End-To-End Verification And Performance A/B

**Files:**
- Modify: `docs/os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers/README.md`
- Create: `docs/experiments/reports/RNVPCE-WORKER-LOCAL-BATCHING-2026-11-15.md`

**Interfaces:**
- Consumes: completed Tasks 1-3 and the existing overwrite sweep harness.
- Produces: correctness evidence and repeatable 4/16/32-thread performance data.

- [x] **Step 1: Run formatting, normal compile, ktest compile, and focused ktests**

  Require zero new compiler errors and all targeted tests to pass; record any unrelated full-suite limitation separately.

- [x] **Step 2: Build one release image and run a smoke test**

  Use `/dev/nvme1n1p2`, 2 GiB low-fragmentation files, a 128 MiB NV pool, 4 KiB random overwrite, four rTIO workers, and four virtqueues. Require sync, unmount, e2fsck, and hctx validation.

- [x] **Step 3: Run five complete 4/16/32-thread rounds with alternating point order**

  Compare mean, median, standard deviation, pages per completion batch, worker page balance, ready-inode dequeues, durability leader attempts/wins, and capacity-wait latency against the current exact-index baseline.

- [x] **Step 4: Record the design and measured outcome**

  Document both successful and rejected behavior. Do not claim improvement unless repeated 32-thread throughput improves beyond baseline variation without regressing 4/16-thread means or correctness.

