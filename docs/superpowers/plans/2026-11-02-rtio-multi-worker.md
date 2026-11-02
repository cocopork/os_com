# rTIO Multi-worker Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run multiple rNVPCe migration submitters without blocking unrelated foreground extents, and coalesce their completed batches behind one device flush per durability epoch.

**Architecture:** A bounded rNVPCe (`RnvpcExtend`) worker pool concurrently claims distinct `DirtyNvExtentEntry` objects and submits asynchronous BIOs under a shared migration gate. One durability coordinator drains all currently ready batches, performs one device flush, and publishes entry-granular reclaim work; sync uses the gate's exclusive side to quiesce the pipeline and a guard-authorized entry point to submit remaining dirty entries.

**Tech Stack:** Rust 2024, Asterinas kernel threads and synchronization, rNVPCe redirect index, EXT4 rTIO, virtio-blk, OSDK ktests, QEMU/KVM, fio, e2fsck.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/020-rtio-multi-worker/README.md`

## Global Constraints

- The first version parallelizes only rNVPCe (`RnvpcExtend`) migration; rNVPC WAL remains single-worker.
- Persistent formats, authority ordering, crash recovery, and the orderless migration protocol do not change.
- Foreground writes never acquire a filesystem-wide migration gate.
- No device I/O or wait occurs while holding a redirect-index spinlock, inode lock, extent-tree lock, allocation-group lock, or NVPage object-table lock.
- One durability epoch performs at most one device flush, without adding a fixed coalescing delay.
- Use test-first development and retain the existing 1024-page budget per worker round.

## Review Focus

- A sync arriving while several workers submit BIOs must prevent later submissions and wait for active submitters without deadlock; Task 1 adds the shared/exclusive gate test and Task 4 adds the sync integration test.
- Two workers starting from the same redirect partition must never claim the same entry; Task 2 adds exact entry-identity assertions.
- One failed BIO in an epoch must not reclaim its entry or corrupt successful-batch accounting; Task 3 adds mixed success/failure tests.
- A failed device flush must retain every epoch entry as NVM-authoritative and release no capacity; Task 3 adds the flush-failure test.
- rNVPC WAL must not accidentally use multiple workers or the rNVPCe epoch path; Task 4 adds a worker-count and legacy-route regression.

---

### Task 1: Add Worker Policy and the Migration Quiescence Gate

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/src/fs/fs_impls/ext4/migration.rs`

**Interfaces:**
- Produces: `rtio_worker_count(mode: RnvpcMode, cpu_count: usize, override_value: Option<usize>) -> Result<usize>`.
- Produces: `MigrationGate::enter_shared() -> MigrationPermit` and `MigrationGate::enter_exclusive() -> MigrationQuiesceGuard`.
- Consumes: existing kernel command-line access and rNVPC mode configuration.

- [ ] **Step 1: Write failing policy and gate tests**

Add tests asserting the 64-CPU default is four workers, the 16-CPU default is two, rNVPC WAL always returns one, overrides 1 and 16 are accepted, and 0/17 are rejected. Add a concurrency test proving two shared permits coexist and an exclusive waiter completes only after both are dropped. The gate must mark itself quiescing before waiting so continuous shared admissions cannot starve sync.

- [ ] **Step 2: Run the focused tests and verify RED**

Run separately:

- `cargo osdk test rtio_worker_policy --qemu-args="-enable-kvm -cpu host -display none"`
- `cargo osdk test migration_gate --qemu-args="-enable-kvm -cpu host -display none"`

Expected: compilation or assertions fail because the policy and gate do not exist.

- [ ] **Step 3: Implement the policy and gate**

Add the exact interfaces above. Store the gate on `Ext4`; shared permits count active submission rounds, while the exclusive guard sets quiescing before waiting, prevents new permits, and waits for the count to reach zero. Parse `rtio_workers=N` once from the kernel command line. Change `start_nv_migration` to return `Result<()>` so `Ext4::open` rejects invalid values before any worker starts.

Validation ruling: the worker pool remains available through
`rtio_workers=2..16`, but the default is one worker. On the current
single-queue virtio device, four submitters increased submission contention
and capacity-wait time. Automatic multi-worker enablement therefore requires
a future multi-queue or centralized-submission design.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run the Step 2 command and require all selected tests to pass.

### Task 2: Start Multiple rNVPCe Submission Workers

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `MigrationGate::enter_shared()` and `rtio_worker_count()` from Task 1.
- Produces: `Ext4::migrate_rnvpce_round(&self, budget: usize) -> Result<usize>` without the legacy global serialization.
- Retains: `Ext4::migrate_nv_round()` as the test/sync entry point and legacy dispatcher.

- [ ] **Step 1: Write failing disjoint-claim and concurrent-round tests**

Add a redirect-directory test that independently claims two sets and asserts no `Arc<DirtyNvExtentEntry>` identity appears twice. Add an EXT4 route test with two separate entries and a test barrier proving both rNVPCe rounds enter submission before either is released. Record candidate claim-lock wait/hold time so the remaining short same-inode map contention is measurable.

- [ ] **Step 2: Run the focused tests and verify RED**

Run separately:

- `cargo osdk test rnvpce_multi_worker_claim --qemu-args="-enable-kvm -cpu host -display none"`
- `cargo osdk test rnvpce_submission_rounds_overlap --qemu-args="-enable-kvm -cpu host -display none"`

Expected: the overlap assertion fails because `nv_migration_lock` serializes both rounds.

- [ ] **Step 3: Split native and legacy round dispatch**

Move the Native claim/BIO construction into `migrate_rnvpce_round`. Acquire one shared migration permit for claim through submission. Keep the legacy rNVPC path behind one exclusive single-worker lock.

- [ ] **Step 4: Spawn the configured Native worker count**

Change `start_nv_migration` to spawn `rtio_worker_count` identical migration loops. Preserve wake-all behavior under capacity pressure and immediate resubmission after a productive round.

- [ ] **Step 5: Run the focused tests and verify GREEN**

Run the Step 2 command and the redirect-index test aggregate.

### Task 3: Coalesce Ready Batches into Durability Epochs

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/batch.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Produces: `Ext4::finish_rnvpce_durability_epoch(&self, batches: Vec<Arc<MigrationBatch>>) -> Result<usize>`.
- Consumes: the existing completion queue, `MigrationBatch::take_jobs`, entry leases, reclaim queue, and global in-flight accounting.

- [ ] **Step 1: Write a failing one-flush-for-two-batches test**

With `rtio=off`, create two independently submitted rNVPCe batches, allow both BIO sets to complete, invoke completion manually once, and assert that both entries reach reclaim-ready state while the test block device records exactly one additional flush. Keeping the background completion worker disabled prevents it from consuming the first batch before the second is ready.

- [ ] **Step 2: Write failing mixed-error and flush-error tests**

Assert that a batch containing a failed BIO is not reclaimed while a successful batch retains correct accounting. Inject a device-flush failure and assert every successful-I/O entry remains NVM-authoritative and no NVPage capacity is released.

- [ ] **Step 3: Run the focused tests and verify RED**

Run: `cargo osdk test rnvpce_durability_epoch --qemu-args="-enable-kvm -cpu host -display none"`

Expected: the two-batch test observes two flushes or the new epoch API is absent.

- [ ] **Step 4: Implement epoch completion**

Drain the ready queue once, partition rNVPCe batches by I/O status, issue one device flush for all successful rNVPCe batches, transition their leases to `IoComplete`, and publish reclaim work. Failed batches retain their entry leases until the epoch decision ends; dropping those leases then restores `Migrating -> Dirty`. Keep rNVPC WAL completion on its existing function.

- [ ] **Step 5: Run the focused tests and verify GREEN**

Run the Step 3 command and require the success, mixed-error, and flush-error cases to pass.

### Task 4: Quiesce Sync and Preserve Legacy Behavior

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `MigrationGate::enter_exclusive()` from Task 1 and the epoch completion API from Task 3.
- Produces: `Ext4::migrate_rnvpce_round_quiesced(&self, guard: &MigrationQuiesceGuard, budget: usize) -> Result<usize>`.
- Produces: sync/unmount behavior that excludes new worker submissions, uses the exclusive-guard entry point for remaining dirty entries, drains all epochs and reclaim work, then exports SSD metadata.

- [ ] **Step 1: Write failing sync-quiescence and legacy-worker tests**

Hold a Native shared round at its test barrier, start sync, and assert export does not begin until the round leaves and completion/reclaim finish. Assert an rNVPC WAL mount reports and starts one migration worker even with `rtio_workers=4`.

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo osdk test rnvpce_sync_quiesces_workers rnvpc_wal_stays_single_worker --qemu-args="-enable-kvm -cpu host -display none"`

Expected: sync can enter through the old mutex contract or worker policy is not enforced.

- [ ] **Step 3: Update sync, pager, shutdown, and drain paths**

Acquire the exclusive migration guard before the final migration/drain/export sequence. Use `migrate_rnvpce_round_quiesced` so sync never recursively requests a shared permit. Remove allocation-metadata pager dependence on the filesystem-wide migration mutex: Native migration does not modify allocation metadata, while foreground allocation and pager writeback retain their group-local coordination. Ensure shutdown wakes and joins logically through existing weak-reference termination conditions without abandoning in-flight accounting.

- [ ] **Step 4: Run the focused tests and the rNVPC route aggregate**

Run: `cargo osdk test ext4_research_routes --qemu-args="-enable-kvm -cpu host -display none"`

Expected: all rNVPCe and rNVPC WAL scenarios pass.

### Task 5: Add Multi-worker Diagnostics

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/pipeline_profile.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Produces counters for configured/active/peak workers, durability epochs, batches and pages per epoch, device flushes, submitted pages/BIOs, maximum in-flight pages, and capacity-wait latency.
- Consumes: existing pipeline-profile and batch-metrics reporting.

- [ ] **Step 1: Write failing counter-accounting tests**

Run two overlapping submission rounds and one two-batch epoch. Assert peak active workers is two, epoch count is one, epoch batch count is two, and flush count is one.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cargo osdk test rnvpce_multi_worker_metrics --qemu-args="-enable-kvm -cpu host -display none"`

Expected: required counters are absent.

- [ ] **Step 3: Implement bounded atomic counters and report output**

Update counters only at stage boundaries; do not add per-page timing calls. Include worker identifier only in diagnostic traces, not persistent state.

- [ ] **Step 4: Run focused and compile verification**

Run the Step 2 command, `cargo osdk check --package aster-kernel`, and `cargo osdk check --ktests --package aster-kernel`.

### Task 6: Correctness and Performance Validation

**Files:**
- Create: `docs/experiments/reports/RTIO-MULTI-WORKER-2026-11-02.md`
- Reuse: `docs/experiments/scripts/preliminary-performance-sweep-20261030/`

**Interfaces:**
- Consumes: release kernel with `rtio_workers=1` and the default worker policy.
- Produces: correctness and performance evidence; no production API.

- [ ] **Step 1: Run complete kernel regression coverage**

Run the full kernel ktest command. Record every failure; do not report completion if unrelated failures are omitted.

- [ ] **Step 2: Build both release configurations**

Build otherwise identical 64-vCPU, 8 GiB, 128 MiB-pool images with `rtio_workers=1` and `rtio_workers=4`. Keep CPU isolation enabled and diagnostic counters disabled for release bandwidth runs.

- [ ] **Step 3: Run the four-point comparison**

On the authorized NVMe partition, run 4 KiB random write and 128 KiB sequential first write at jobs 1 and 16, 2 GiB each. Reformat before each point and run read-only `e2fsck` afterward.

- [ ] **Step 4: Run one instrumented capacity-pressure point**

Run 128 KiB sequential first write at 16 jobs with diagnostics. Record capacity-wait time, candidate and submit latency, worker concurrency, in-flight depth, durability epoch size, flush count, and reclaim throughput.

- [ ] **Step 5: Write the report**

State configuration, raw artifact paths, hashes, correctness results, exact metrics, and whether reduced capacity waiting explains any throughput change. Explicitly distinguish evidence from inference and note that rNVPC WAL was not performance-parallelized.
