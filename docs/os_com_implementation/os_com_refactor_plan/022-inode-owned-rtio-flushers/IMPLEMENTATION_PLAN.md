# Inode-owned rTIO Flushers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rNVPCe's global candidate scan and dedicated completion thread with inode-owned flusher pipelines while preserving one device FLUSH per durability epoch.

**Architecture:** `RedirectDirectory` assigns each active inode to one flusher and queues that inode only for its owner; ownership uses per-inode state and atomic per-worker load counters. BIO callbacks publish to owner-local queues, an elected flusher performs the device-wide durability barrier, and the original owner performs entry completion and reclaim without a dedicated completion thread.

**Tech Stack:** Rust `no_std`, Asterinas kernel threads/wait queues/atomics/spinlocks, rNVPCe redirect entries, virtio-blk queue affinity, OSDK ktests, QEMU/KVM and fio.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers/README.md`

## Global Constraints

- Apply only to `RnvpcExtend`; rNVPC WAL remains on its existing single-worker completion path.
- Do not change the persistent redirect format, soft-update order, NVM authority or recovery format.
- A single inode has exactly one owner while any dirty/claimed/in-flight/completion/reclaim entry exists.
- Ownership checks must not acquire a global pool lock or VFS/extent-tree inode read/write lock.
- Never hold a per-inode scheduling/entry lock while submitting BIO, waiting for I/O or executing device FLUSH.
- BIO callbacks only record status, enqueue the completed batch and wake its owner.
- One closed durability epoch issues at most one device FLUSH; FLUSH failure retains NVM authority.
- No arbitrary work stealing and no same-inode subdivision in this implementation.
- Historical implementation constraint: the first validation retained one
  default worker. Superseded on 2026-11-05: rNVPCe now defaults to
  `min(4, submission_queue_count)`; explicit `rtio_workers=N` still overrides it.
- Preserve the untracked `kernel/test` symlink and do not track raw experiment artifacts.

## Review Focus

- A dirty entry published while the previous final entry is being reclaimed must retain one valid owner and one queue notification; Task 1 adds a publication-versus-release race test.
- A worker losing durability leadership must continue useful local work without waiting on the leader lock; Task 3 adds a concurrent nonleader-progress test.
- Data BIO failure mixed with successful batches must not let failed entries cross the successful epoch barrier; Task 3 extends mixed-status tests.
- sync/unmount racing with a callback must drain owner queues exactly once without submitting after exclusive quiescence; Task 4 adds a callback-at-quiesce test.
- A batch arriving after a leader closes its epoch must trigger a leadership handoff and cannot sleep indefinitely; Task 3 adds a post-snapshot-arrival test.
- Recovery-created dirty entries that predate worker registration must be assigned and queued exactly once; Task 2 adds a recovery-registration test.

---

### Task 1: Per-inode ownership and load-balanced scheduling

**Files:**
- Create: `kernel/src/fs/rnvpc/redirect_index/ownership.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`

**Interfaces:**
- Produces: `RedirectDirectory::configure_migration_workers(fs_id: u64, workers: usize) -> Result<()>`.
- Produces: `RedirectDirectory::owner_worker(fs_id: u64, inode: u64) -> Option<usize>`.
- Produces: `RedirectDirectory::claim_ready_entries_for_worker(fs_id: u64, worker_id: usize, page_budget: usize) -> Result<Vec<NvExtentEntryLease>>`.
- Produces: per-worker `MigrationWorkerLoadSnapshot` with queued, in-flight, completion-pending and owned-inode counts.
- Consumes: existing `DirtyNvExtentEntry` state transition and `InodeRedirectMap::entries`.

- [ ] **Step 1: Write failing ownership tests**

Add ktests proving: least-loaded assignment with rotating tie-break; stable owner across multiple entries of one inode; different inodes distribute across workers; publication racing final reclaim never produces unowned active work; a worker can claim only its inode; and claiming one inode does not retain another inode's map lock.

- [ ] **Step 2: Run the focused tests and verify RED**

Run each new test with `cargo osdk test <name> --qemu-args="-enable-kvm -cpu host -display none"` from `kernel` in the recovery tools container.

Expected: compilation failure on the absent ownership APIs or behavioral failure showing global/unowned claiming.

- [ ] **Step 3: Implement per-inode ownership**

Add a short-held per-inode migration state containing owner, generation, active/queued/in-flight/completion/reclaim accounting and ready-queue membership. Use atomic per-worker page loads for selection; protect final release versus new publication with only the target inode's migration state. Refactor map lookup so the partition lock is released before the inode entry-map scan.

- [ ] **Step 4: Implement worker-specific ready-inode claim**

Dirty publication enqueues the inode once for its owner. Worker claim pops only its local inode handles, scans only those inode entry maps, transitions `Dirty -> Migrating` up to the page budget and requeues an inode if dirty entries remain. Expose wrappers on `Rnvpc` in `frame/redirect.rs`.

- [ ] **Step 5: Run focused and aggregate rNVPC tests**

Run the new tests plus `cargo osdk test ext4_research_routes --qemu-args="-enable-kvm -cpu host -display none"`.

Expected: all new tests pass and all existing research markers remain green.

- [ ] **Step 6: Commit**

```bash
git add kernel/src/fs/rnvpc/redirect_index.rs kernel/src/fs/rnvpc/redirect_index/ownership.rs kernel/src/fs/rnvpc/frame/redirect.rs
git commit -m "Assign rTIO inodes to migration workers"
```

### Task 2: Publication, recovery and owner wake integration

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: Task 1 ownership configuration, lookup and worker-specific claim.
- Produces: `Ext4::request_nv_migration_for_inode(inode: u64)` and owner-specific worker wakeup.
- Produces: recovery registration that queues pre-existing dirty entries once.

- [ ] **Step 1: Write failing integration tests**

Add tests proving that foreground range publication wakes only the assigned worker, two writes to one inode preserve the owner, pre-registration recovery entries are assigned once when workers start, and different inode publication does not take the removed filesystem-wide `nv_inodes` mutex.

- [ ] **Step 2: Run focused tests and verify RED**

Expected: failure because owner-local triggers and recovery registration are absent.

- [ ] **Step 3: Integrate publication and recovery**

Configure the redirect scheduler before starting rNVPCe workers; rebuild assignments once for already recovered dirty entries. Replace `nv_inodes: Mutex<BTreeSet<_>>` foreground insertion with owner notification. Add one wait queue per rNVPCe worker; pressure/sync may wake all, ordinary publication wakes only the owner.

- [ ] **Step 4: Route migration rounds by owner**

Change rNVPCe `migrate_nv_round_on_queue` to pass `worker_id` into Task 1's claim API. Preserve WAL's existing global candidate path and queue behavior.

- [ ] **Step 5: Run integration and aggregate tests**

Run the new tests, `rnvpce_workers_keep_stable_distinct_queue_ids`, and `ext4_research_routes`.

Expected: owner-local wake/claim tests and the full research aggregate pass.

- [ ] **Step 6: Commit**

```bash
git add kernel/src/fs/fs_impls/ext4 kernel/src/fs/rnvpc
git commit -m "Route dirty inodes to their rTIO flusher"
```

### Task 3: Owner-local completion queues and durability leadership

**Files:**
- Create: `kernel/src/fs/fs_impls/ext4/migration/worker.rs`
- Create: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/batch.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: Task 2 stable `worker_id -> queue_id` and local worker triggers.
- Produces: `RtioWorkerRuntime`, owner-local data-complete/durable-result queues and counters.
- Produces: nonblocking `DurabilityArbiter::try_advance(&Ext4) -> Result<usize>`.
- Produces: `MigrationBatch::owner_worker_id() -> usize` for rNVPCe batches.

- [ ] **Step 1: Write failing completion-routing tests**

Add tests proving callbacks route batches only to their immutable owner, two owners' ready batches share one FLUSH, results return to the correct owners, mixed data failure never becomes durable, a worker that loses leader election can submit/process local work without waiting, and a batch arriving after the leader snapshot causes another epoch without an unrelated wakeup.

- [ ] **Step 2: Run focused tests and verify RED**

Expected: failure because all callbacks still publish to the global completion queue and the dedicated coordinator owns the FLUSH.

- [ ] **Step 3: Add owner-local runtime queues**

Give every rNVPCe batch an owner ID and every worker separate IRQ-safe data-complete and durable-result queues. Callback completion only records status, enqueues to that owner and wakes it. Keep the old global completion queue for WAL only.

- [ ] **Step 4: Add the durability arbiter**

Use nonblocking leader election. The leader snapshots all currently ready owner queues without retaining their locks, separates failed batches, performs one synchronous device FLUSH for successful batches, and publishes success/failure results back to owner queues. Maintain an atomic ready-batch count and arbiter generation; release with pending work explicitly wakes a successor, and workers recheck both before sleeping. No inode, entry-map or worker-queue lock may be held over FLUSH. A data BIO error restores NVM authority and latches the existing mount-wide migration failure instead of entering an unbounded retry loop.

- [ ] **Step 5: Run cross-queue durability tests**

Run the new tests and the existing `rnvpce_cross_queue_*` fault tests individually, followed by `ext4_research_routes`.

Expected: one FLUSH per successful epoch, exact owner routing, failure authority and all aggregate markers pass.

- [ ] **Step 6: Commit**

```bash
git add kernel/src/fs/fs_impls/ext4/migration kernel/src/fs/fs_impls/ext4/migration.rs kernel/src/fs/fs_impls/ext4/fs.rs kernel/src/fs/fs_impls/ext4/nv_tests.rs
git commit -m "Distribute rTIO completion to flusher workers"
```

### Task 4: Owner post-processing, reclaim and quiescence

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/worker.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/reclaim.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: Task 3 owner-local durable results.
- Produces: owner-only entry completion/reclaim and exact load-counter release.
- Produces: sync/unmount drain over all owner runtimes without a dedicated rNVPCe completion thread.

- [ ] **Step 1: Write failing owner and quiescence tests**

Add tests proving only the owner transitions/reclaims an entry, final reclaim releases assignment exactly once, callback racing exclusive quiescence is drained, sync waits for all local queues/counters, and FLUSH failure leaves NVM authority plus balanced ownership/load accounting.

- [ ] **Step 2: Run focused tests and verify RED**

Expected: owner assertions or drain assertions fail on the current centralized completion/reclaim path.

- [ ] **Step 3: Move post-processing to the owner loop**

Split current `finish_rnvpce_durability_epoch`: the arbiter decides durability only; owner workers perform `Migrating -> IoComplete`, redirect retirement, NVPage range release and capacity wakeup. Update per-inode/per-worker counters at every state transition and release the owner only after the last reclaim-pending entry exits.

- [ ] **Step 4: Remove the rNVPCe completion thread**

Spawn the legacy completion thread only for WAL. rNVPCe worker loops drain local durable results, advance durability, submit owned candidates and sleep on owner-local triggers. Keep failure cleanup owner-routed.

- [ ] **Step 5: Integrate sync/unmount drain**

Under the existing exclusive migration gate, stop new worker submissions. The sync context then uses each owner's worker-specific claim path and `Queue(i)` to submit dirty work that was already queued before quiescence, without changing ownership or scanning globally. After queued dirty reaches zero, drain data completions through the arbiter, drain owner post-processing/reclaim, and require every worker counter/queue to reach zero before native metadata export.

- [ ] **Step 6: Run full correctness verification**

Run focused tests, all block and virtio ktests, `ext4_research_routes`, `cargo osdk check --package aster-kernel`, `cargo osdk check --ktests --package aster-kernel`, and formatting checks.

Expected: all suites pass; no rNVPCe dedicated completion thread remains; WAL tests remain unchanged.

- [ ] **Step 7: Commit**

```bash
git add kernel/src/fs/fs_impls/ext4 kernel/src/fs/rnvpc
git commit -m "Complete rTIO migration in inode owners"
```

### Task 5: Diagnostics and performance validation

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/pipeline_profile.rs`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers/README.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/README.md`
- Create: `docs/experiments/reports/RTIO-INODE-OWNED-FLUSHERS-2026-11-03.md`
- Reuse: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/`

**Interfaces:**
- Consumes: Task 4 worker counters, queues and durability arbiter.
- Produces: mount-local owner/batch/completion metrics and retained experiment report.

- [x] **Step 1: Write failing diagnostics tests**

Add tests for owned-inode counts, ready-inode dequeue, per-worker batch pages, local queue depths, leader attempts/wins, owner completion/reclaim and assignment release. Snapshot reads must not take inode or pool-wide locks.

- [x] **Step 2: Run focused tests and verify RED**

Expected: failure on absent fields or counters.

- [x] **Step 3: Implement diagnostics and update the parser**

Add mount-local counters and extend the existing report parser without changing raw-data tracking policy. Keep counters disabled outside profiling/ktest where current metrics are disabled.

- [x] **Step 4: Run non-destructive validation first**

Run all Task 4 checks plus harness self-tests. Build and fingerprint one profiling-enabled release image; restore the profiling-only source flag after copying the ISO.

- [x] **Step 5: Run the authorized serialized real-device matrix**

On the already authorized `/dev/nvme1n1p1` (or p2 only if p1 is occupied and p2 passes all guards), run 1x1 and explicit 4x4 configurations for 4 KiB random and 128 KiB sequential first writes at jobs 1 and 16, 2 GiB each. Reformat each point, require fio/sync/unmount success and read-only `e2fsck -fn`, and retain raw artifacts outside Git.

- [x] **Step 6: Evaluate and document**

Compare throughput, latency, capacity wait, batch-page distribution, owner balance, local completion queues, durability FLUSH and hctx usage. A single-file point may use one worker; multi-file points must demonstrate cross-inode distribution. Update 022 and the refactor index with exact pass/fail status, without changing the one-worker production default based on one instrumented run.

- [x] **Step 7: Commit tracked evidence**

```bash
git add kernel/src/fs/fs_impls/ext4/pipeline_profile.rs \
  docs/os_com_implementation/os_com_refactor_plan/022-inode-owned-rtio-flushers \
  docs/os_com_implementation/os_com_refactor_plan/README.md \
  docs/experiments/reports/RTIO-INODE-OWNED-FLUSHERS-2026-11-03.md
git commit -m "Validate inode-owned rTIO flushers"
```
