# rNVPCe Soft-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the existing `rnvpc_extend` prototype into the final WAL-free rNVPCe path where every successful write is durable in NVPool and recoverable through soft-update ordering.

**Architecture:** NVPool stores committed data pages and persistent EXT4 metadata objects. Foreground writes reserve only an SSD block count and publish initialized NV mappings; they do not choose final SSD addresses. A global dirty-NVPage queue feeds an asynchronous rTIO migration thread, reverse locators identify exact extent slots without tree walks, and a separate process-context completion thread publishes SSD mappings and reclaims NVPages after durable I/O.

**Tech Stack:** Rust, Asterinas kernel, EXT4, NVPool DRAM persistence model, virtio-blk, kernel ktests, QEMU/FIO.

**Spec:** `os_com_implementation/rNVPC-rTIO/design.md`.

## Global Constraints

### Foreground repair-workspace optimization (2026-10-13, phase 1 complete)

- Preserve the existing full-image local mutation recovery protocol in phase 1.
- Reuse a bounded pool of allocated, uncommitted metadata frames, renewing their
  allocation sequence on reuse; no permanent per-inode frame reservation.
- Bypass the generic metadata lookup index for repair images, which are addressed
  by persistent object reference. Keep the pending mark indexed for recovery and
  runtime retry, and clean it by the known reference before recycling images.
- Validate repeated reuse, stale references, interrupted rewrite/recovery and
  image reclamation. Run over-capacity KVM fio on the authorized NVMe partition.
- Compare the uninstrumented baseline and optimized bandwidth using identical
  workloads; then compare foreground phase counters. Phase 2 requires measured
  phase-1 improvement and must retain a recovery path for multi-field updates.

Validation: two targeted KVM recovery/reuse/capacity tests and five optimized fio
runs pass. Extent phase falls to 71-119us/write from 289-302us; busy bandwidth
sample medians are79.50->98.84MiB/s, with one low optimized sample28.13 retained.
See `test/aster-ext4-baseline-test/results/20261013-repair-workspace/README.md`.
Phase 2 compact-entry/append protocols remain separate from this verified change.

- rTIO completion regrouping uses at most 128 pages per inode publication window,
  refreshes reverse locators under that window, and persists each touched leaf once
  within the group; successful leaves reclaim independently of failed leaves.

- `rnvpc_extend` remains WAL-free; WAL/checkpoint counters must remain zero.
- Without the NVPool mount option, existing EXT4 write/fsync/mmap semantics remain unchanged.
- With rNVPCe enabled, buffered, synchronous, and direct writes share the same direct-to-NV durable path; mmap returns an unsupported-operation error.
- A successful write is durable; `fsync` must not add SSD writeback or `block_device.sync()` work for already committed rNVPCe data.
- rNVPCe bypasses PageCache for both reads and writes and does not allocate a DRAM data-page or bounce-buffer copy.
- `copy_from_user` writes directly into pinned NVPage ranges; a nonzero copied prefix is persisted and returned as a short write.
- `committed` means an object's persistent contents are valid; `mapped` means the current inode extent tree selects that exact generation; `reachable` is the corresponding recovery-traversal property.
- Persistent EXT4 metadata has one durable NVPool copy; DRAM structures are rebuildable indexes, not duplicate persistent trees.
- Extent updates are local. Whole-tree flatten/reserialize operations are forbidden on the rNVPCe write path.
- Persistent references use `(slot, allocation_seq)` to prevent slot-reuse ABA and are published directly through the final extent and inode structures.
- A compact DRAM reverse index maps each mapped NVPage to its current leaf extent slot; it is rebuilt after restart and is never treated as persistent truth.
- During an extent split, only NVPages in the moved logical interval are frozen against rTIO migration and reclamation; unrelated pages remain eligible.
- Each inode has one persistent `SpaceReservationRecord`; the global outstanding-reservation total is volatile and rebuilt after restart.
- New NV mappings have no predetermined SSD address. Existing SSD-backed overwrites may retain their already allocated block.
- Candidate discovery uses a duplicate-suppressing global dirty-NVPage queue and never traverses inode extent trees.
- BIO end-I/O performs only bounded queueing and atomic accounting; persistent extent and allocation updates execute in the completion thread.
- No ordinary page, inode, or extent lock may remain held across asynchronous BIO execution.
- A write interrupted before returning may leave a safe persistent prefix and unreachable allocations; a successful return means the complete syscall range is durable.
- Persistence work follows the syscall range and splits only at the supported extent-size boundary.
- Real NVDIMM power-loss testing is out of scope; fault injection validates protocol ordering against the DRAM durable-image model.

---

### Task 1: Stabilize asynchronous rTIO completion (complete)

**Files:**
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `os_com_implementation/test/aster-ext4-baseline-test/test_rnvpc_completion_context.py`

**Interfaces:**
- Consumes: `BioCompleteFn`, `FlushRunPage`, `rnvpc::complete_flush`.
- Produces: the existing bounded, preallocated completion queue; Task 6D moves its normal drain from `rnvpc_flush_loop` to the dedicated completion loop.

- [x] Preallocate capacity for `RNVPC_MAX_INFLIGHT_BIOS` before any BIO can complete.
- [x] Restrict BIO callbacks to bounded queue insertion, atomic accounting, and wakeups.
- [x] Move eviction completion and `rnvpc::complete_flush` into process context.
- [x] Pass the focused completion-context contract tests and normal kernel compilation.
- [x] Commit the isolated fix as `307f34b75`.

### Task 2: Build final-form persistent metadata and fixed inode roots (redesign required)

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/metadata.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/mod.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: NVBH allocation, per-object commit words, persistence fences, and stable NV object identifiers.
- Produces: typed final-form allocation, extent, inode, and mutation records addressed by stable `(slot, allocation_seq)` references.

- [x] Define self-describing persistent metadata headers and strict typed-payload validation (`0adbb6ca1`; reusable).
- [x] Add NVBH helpers that persist and decode typed metadata without creating complete DRAM metadata copies (`0adbb6ca1`; reusable).
- [x] Remove obsolete batch-publication metadata layouts and APIs from the rNVPCe source.
- [x] Give each persistent inode root a stable location that recovery can discover directly.
- [x] Support ordered in-place persistence of fixed-inode visibility fields with atomic aligned stores; Task 3 owns operation-specific extent-node updates that avoid torn committed contents.
- [x] Retain generation-checked persistent references and per-slot allocation sequences as the only slot-reuse ABA defense.
- [x] Replace legacy batch-publication tests with corruption, in-place persistence-cut, round-trip, and modeled-restart tests.

### Task 3: Implement persistent metadata and local in-place soft updates (partially reusable)

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/extent_mutation.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/inode/extent_manager/`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: typed NV metadata records and fixed persistent inode roots from Task 2.
- Produces: persistent NV inode/extent/reservation objects, mapped NVPages, and per-inode `PendingExtentMutation` records for local tree changes.

- [x] Prove that dropping DRAM indexes does not lose persistent metadata (`79859cc6b`; reusable exact-reference and modeled-restart infrastructure).
- [x] Route rNVPCe extent, inode, and allocation access through stable NV object identifiers (`d5d44f9ba`; generation-checked access is reusable).
- [x] Keep ES tree and extent cache as rebuildable DRAM indexes; do not retain a second complete extent tree (`158725f74`; reusable).
- [x] Keep a lightweight mutation mark durable until local split repair completes (`e8e557ffb`; Prepared/ParentPublished phases and topology checks are reusable).
- [x] Remove flatten/reserialize from the rNVPCe mutation path (`e8e557ffb`; local traversal and split machinery are reusable).
- [x] Replace immutable path-local metadata CoW and inode-root replacement with ordered in-place leaf, parent, and inode updates.
- [x] Overwrite an already resident NVPage in place and leave its extent reference unchanged; use `(slot, allocation_seq)` only to validate identity, not to create a data version.
- [x] Allocate a new NVPage only for a hole, a newly allocated block, SSD-to-NV promotion, or relocation; fully initialize it before publishing the extent reference.
- [x] Represent each complex mutation with one per-inode mark containing the parent identity, old/new child `(slot, allocation_seq)` references, split boundary, operation kind, and `Prepared`/`ParentPublished` phase.
- [x] For splits, persist the new child and temporary repair images, persist `Prepared`, publish the parent reference, persist `ParentPublished`, shrink the old child range, verify both topology facts, and clear the mark last.
- [ ] Derive the moved interval `[split_logical_block, old_child_end)` at split start and register a range freeze before publishing `Prepared`.
- [ ] Keep reverse entries in the frozen interval non-empty but non-migratable while old and new children may overlap; after shrinking the old child, retarget them to the final new-child slots before clearing the mark and releasing the freeze.
- [ ] Use one range-aware mutation gate for split repair, reverse updates, rTIO candidate admission, and reclaim checks; do not freeze the whole inode.
- [x] Make every step idempotent: recovery rolls repair images forward and recognizes an already-published parent or already-shrunk old child without reversing a completed update.
- [x] Protect fixed-inode root growth with `RootSplit`; recovery rolls every root-publication crash image forward, repairs a reused old root idempotently, and clears the mark last.
- [x] Update inode block count, size, and generation last so every crash cut exposes only initialized extent coverage.
- [x] Preserve per-inode mutation exclusion, cross-inode concurrency, and native EXT4 mode isolation (`e8e557ffb`; reusable).

### Task 4: Integrate ordered in-place soft updates into VFS writes (migration required)

**Files:**
- Modify or replace: `os_com/kernel/src/fs/rnvpc/extend_cow.rs` (remove data-CoW semantics)
- Modify: `os_com/kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/impl_for_vfs/inode.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: Task 2 fixed inode roots, Task 3 local metadata updates, EXT4 block allocation, and inode serialization.
- Produces: one syscall-sized synchronous soft-update path that returns only after allocation, initialized data, extent references, and inode visibility fields are durable.

**Current progress:** Complete. Production VFS reads and writes use the syscall-sized `RangeWriteBatch` path; legacy full-page `WriteBatch` helpers remain only as extent-mutation test fixtures. rNVPCe no longer fills, dirties, composes, resizes, or invalidates PageCache data on `read()`/`write()`.

- [x] Add first-write, overwrite, partial-page, and sparse multi-page syscall tests; adapt their expected path to direct NV access.
- [x] Existing implementation allocates final SSD blocks before NV reference publication; Task 6A deliberately replaces this completed legacy behavior with count-only reservation.
- [x] Add pinned range operations that copy directly from user memory into NVPage storage, zero NVPage ranges, accept SSD reads directly into NVPage storage, and persist only the modified range.
- [x] For an existing NV-resident page, serialize against rTIO, copy in place, persist the copied prefix, and return full or short-write length without changing the extent.
- [x] For a hole or new block, zero and initialize the NVPage directly; for an SSD-resident partial page, read SSD data directly into the NVPage before overlaying user bytes; publish the extent only after the page is complete and durable.
- [x] Persist initialized new NVPages, then update local extent references in place, then persist inode visibility fields directly.
- [x] Treat pre-return failures as safe-prefix operations: return the durable copied prefix, release resources that remain private, and leave published but unreachable allocations for recovery.
- [x] Route rNVPCe reads directly through the current NV mapping or SSD without filling PageCache.
- [x] Remove rNVPCe PageCache fill, dirtying, page-composition, and invalidation hooks.
- [x] Route direct and synchronous writes through the durable path; route direct reads through the current NV mapping and reject mmap on rNVPCe mounts.
- [x] Make rNVPCe `fsync` and shared inode-sync entry points avoid SSD writeback and device flush.
- [x] Preserve native EXT4 write/fsync/mmap behavior behind mode-gated VFS defaults.
- [x] Add final range-transaction restart tests and rerun focused range, buffered, direct, synchronous, sparse, and complete rNVPC kernel test filters.

### Task 5: Close crash recovery and leak reclamation

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/recover.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/extent_mutation.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: persistent descriptors, fixed inode roots, generation-checked references, and pending extent mutations.
- Produces: reconstructed DRAM indexes, completed mutations, and a set of unreachable objects eligible for reclamation.

**Current progress:** Complete. Recovery reconstructs exact descriptor identities, repairs every pending local mutation before exposure, traces reachability from fixed inode roots, retains only reachable extent/NVPage objects, and reclaims incomplete write prefixes. ES-tree and extent-cache state remain bounded DRAM accelerators and rebuild lazily from the validated persistent root.

- [x] Add modeled-restart tests at every allocation, data, extent-reference, pending-mark, and inode-field durability edge from Tasks 2 and 4.
- [x] Scan and validate self-describing NV objects without trusting stale DRAM handles; exact allocation-sequence validation is reusable.
- [x] Rebuild reachable NVPage residency from fixed persistent inode roots; leave ES-tree and extent-cache state empty for bounded lazy reconstruction from the same validated roots.
- [x] Rebuild the compact reverse extent-slot index during the same root traversal, reject duplicate final ownership after pending mutations are repaired, and enable rTIO/reclaim only after the index is complete.
- [x] On restart, derive each split freeze from `PendingExtentMutation`, roll the split forward, install final reverse locators, and remove the transient freeze before exposing the inode.
- [x] Require each reachable logical `PageKey` to resolve to exactly one `(slot, allocation_seq)` generation; reject ambiguous cross-leaf mappings instead of choosing an arbitrary index entry.
- [x] Propagate each internal-node routing interval during recovery and reject child nodes or leaf extents that escape their parent range, including static corruption with no pending mutation mark.
- [x] Enumerate pending marks before exposing recovery, validate their generation-checked repair images, and inspect both durable facts: whether the parent references the new child and whether the old child has been shrunk to the split boundary.
- [x] Recover `RootSplit` marks across pre-publication, dependency-only, visible-root, and fully persisted fixed-inode root images.
- [x] Roll `Prepared` forward from its repair images whether parent publication had not started or became durable before the phase update.
- [x] For `ParentPublished`, restore an overlapping or torn old child from its repair image, clear an already-complete mark idempotently, and reject a topology that contradicts the recorded mutation.
- [x] Trace liveness from fixed inode roots through extent nodes and NVPages; reclaim abandoned promotion/relocation pages and leaked allocations before foreground exposure, and fail recovery on an unexpected busy object instead of silently leaking it.
- [x] Fault-test every cut between new-child initialization, `Prepared`, parent publication, `ParentPublished`, old-child shrink, and mark clear; repeated recovery must not leak or double-free frames.

### Task 6A: Replace concrete write-time allocation with capacity reservation

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/metadata.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/descriptor.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Produces: `SpaceReservationRecord { reserved_blocks, consumed_blocks }`, `reserve_ssd_capacity(fs_id, inode, blocks)`, and `consume_ssd_capacity(fs_id, inode, blocks)`.
- Replaces: new-page `block_group`, `first_ssd_block`, and descriptor `ssd_backing_block` ownership established by the foreground write.

- [x] Add tests proving that a new NV mapping commits with a block-count reservation but no final SSD address, while an SSD-backed overwrite preserves its existing allocation without adding credit.
- [x] Replace new-write `AllocationRecord` ownership with one stable per-inode `SpaceReservationRecord`; update it under the existing per-inode publication exclusion and persist reservation growth before publishing a new NV extent.
- [x] Add an atomic global outstanding-reservation counter used by EXT4 admission: `free_blocks - metadata_reserve - outstanding_reservations` must cover every new request before `write()` modifies NV state.
- [x] Remove final SSD addresses from new-page `RangeWriteBatch`, `WriteAllocation`, `WritePage`, and `NvDescriptor` construction; retain an optional existing SSD block only for a promoted overwrite that already owns it.
- [x] Make rollback release private reservation increments, while a published safe prefix retains exactly its corresponding credit.
- [ ] Run the focused reservation tests inside a VM and commit this migration separately; normal and ktest configuration compilation pass.

### Task 6B: Add the global dirty queue, reverse locator, and split-range gate

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/index.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/extent_mutation.rs`
- Delete: `os_com/kernel/src/fs/rnvpc/rtio.rs` after replacing its inode-scanning prototype
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Produces: `enqueue_dirty_nvpage(page, allocation_seq)`, `collect_dirty_nvpages(budget)`, `reverse_lookup(page) -> Option<ReverseExtentRef>`, and `claim_migration_page(page, allocation_seq) -> MigrationLease`.
- `ReverseExtentRef` is the compact 8-byte DRAM locator for the owning leaf extent slot; `MigrationLease` pins one exact page generation without retaining an extent lock across I/O.

- [x] Add tests for duplicate dirty transitions, stale queue generations, direct reverse lookup, concurrent foreground claim, and split-range deferral.
- [x] Insert an NVPage once when it transitions to `NvDirty`; tolerate stale queue entries and discard them by state plus allocation-sequence validation instead of deleting arbitrary interior queue nodes.
- [x] Install, replace, and clear reverse entries atomically with their forward extent changes under the shared page/extent mutation exclusion.
- [x] Register `[split_logical_block, old_child_end)` before `PendingExtentMutation(Prepared)`, defer only reverse locators in that interval, retarget moved locators after old-child repair, and release the range after mark clear.
- [x] Replace inode-scanning production candidate discovery with `collect_dirty_nvpages()`; candidate discovery does not call `mapped_nv_page_ref()` or traverse an inode root.
- [ ] Run the focused queue/reverse/split tests inside a VM and commit this index layer separately.

### Task 6C: Implement the migration submitter and migration-time SSD placement

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/descriptor.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`
- Test: `os_com_implementation/test/aster-ext4-baseline-test/test_rnvpc_completion_context.py`

**Interfaces:**
- Produces: `rtio_migration_loop()`, `validate_reverse_reference(&MigrationLease)`, `allocate_migration_extent(count)`, and `submit_migration_bio(MigrationBatch)`.
- `MigrationBatch` owns the page leases, descriptor-local transient SSD targets, BIO buffers, and per-inode reservation debits until completion processing takes ownership.

- [ ] Add a runtime test proving that one migration pass keeps submitting until the BIO/page budget is full; code inspection and static tests confirm that submission does not wait for completion.
- [x] Claim candidates from the global queue, reject frozen or stale reverse locators, validate the exact forward/reverse identity, and hold no extent lock across SSD I/O.
- [x] Batch unplaced pages, request the largest available contiguous SSD run, partition that run across candidates, and fall back to smaller runs without changing candidate-discovery order.
- [x] Persist each descriptor's transient target and migration phase before submission; no standalone migration log, anchor, or manifest segment is used.
- [x] Transition exact generations `NvDirty -> InFlight`, build merged BIOs up to the configured page and BIO budgets, and transfer all leases to the completion payload.
- [x] Replace the old inode-scanning rTIO module while retaining the bounded asynchronous `rnvpc_flush_round()`/`submit_flush_run()` engine as the migration submitter.
- [ ] Run focused migration-placement tests inside a VM and commit the submitter separately.

### Task 6D: Separate the end-I/O callback from the completion worker

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/write_transaction.rs`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`
- Test: `os_com_implementation/test/aster-ext4-baseline-test/test_rnvpc_completion_context.py`

**Interfaces:**
- Produces: `rtio_bio_end_io(CompletionRecord)`, `rtio_completion_loop()`, `complete_migration_batch()`, `publish_ssd_extent_by_reverse()`, and `retry_failed_migration()`.
- The callback may enqueue, update atomics, and wake only; the completion worker owns all persistent metadata transitions and reclaim.

- [x] Extend the static completion-context test to reject extent mutation, allocator calls, persistence operations, reservation updates, and NVPage reclaim from the BIO callback.
- [x] Keep the callback bounded and allocation-free by transferring the owned completion payload into a preallocated queue and waking a dedicated completion thread; queue entries remain charged against the in-flight budget until drained.
- [x] On success, batch the required SSD durability boundary, reacquire local mutation gates, and validate each reverse locator, exact NV generation, transient target, and current forward reference.
- [x] Persist SSD bitmap/GDT allocation before publishing `Nv(reference) -> Ssd(pblock)`; persist required inode/native metadata before consuming reservation credit.
- [x] Clear reverse and transient migration state only after the SSD extent is durable, then release the lease and reclaim the NVPage.
- [x] On I/O or metadata failure, retain the durable SSD claim and reservation credit when necessary, transition the exact generation back to `NvDirty`, and requeue it without losing the NV copy; retry reuses the same target.
- [x] Split migration submission and completion into independently wakeable loops; the bounded queue and delayed credit return prevent completion lag from overflowing the callback queue.
- [ ] Run focused callback/completion/error-injection tests inside a VM and commit the completion pipeline separately; the static callback test passes.

### Task 6E: Recover reservations, reverse state, and interrupted migrations

**Files:**
- Modify: `os_com/kernel/src/fs/rnvpc/recover.rs`
- Modify: `os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `os_com_implementation/rNVPC-rTIO/implementation.md`
- Modify: `os_com_implementation/STATUS.md`
- Test: `os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: fixed inode roots, `SpaceReservationRecord`s, descriptor-local migration phases, repaired `PendingExtentMutation`s, and persistent SSD allocation metadata.
- Produces: a validated reverse index, global dirty queue, reservation total, and no unresolved migration before rTIO starts.

- [ ] Add modeled-crash tests before/after descriptor target persistence, BIO completion, SSD allocation persistence, extent publication, reservation consumption, reverse clear, and NVPage reclaim.
- [x] Repair every pending extent mutation first, then traverse fixed-inode trees once to rebuild reverse locators, mapped NV generations, and the global dirty queue.
- [x] Sum all per-inode reservation records into the global admission counter and reconcile over-reservation from the recovered forward graph.
- [x] For an interrupted migration whose extent still selects NV, preserve its descriptor-local SSD target and requeue the NVPage; an already published SSD extent makes the old NV descriptor unreachable and reclaimable.
- [x] Reject duplicate reverse ownership and stale allocation sequences, and require every recovered NV extent to receive one reverse locator before rTIO starts.
- [ ] Run repeated-recovery, pool-full, split-concurrency, long slot-reuse, image-backed mount/unmount, and data-verification tests.
- [ ] Run the retained FIO matrix with diagnostic counters disabled, report `busy_bw_mib_s`, queue occupancy, merge size, and completion backlog, then update implementation/status documents with exact results and commits.

**Current limitation:** once a fixed rNVPCe persistent inode root exists, `resize` returns
`EOPNOTSUPP`. Native EXT4 truncate cannot be reused safely because it may free an
SSD allocation or reservation still represented by the persistent NV extent tree. Full support needs
a generation-checked suffix deletion that removes NV references, updates the fixed
inode size, reconciles native SSD blocks and capacity credit, and only then reclaims NVPages.

### NVPool object-state sharding (2026-10-11)

- Scope: move `records + reverse` into 64 slot-indexed DRAM shards; retain
  control-state locking for allocation, capacity admission, indexes, dirty queue,
  frozen ranges. Publication ownership was subsequently moved to inode-local
  synchronization as described below.
- Keep allocation sequence, per-object pins, persistence exclusion, lifecycle
  state and reverse references under the same shard lock. Only the diagnostic
  aggregate pin count becomes atomic; it is not a reclamation/admission predicate.
- Lock order: control state before one object shard; never nest object shards.
  Drop spinlocks before persistence, user copying, waiting or device I/O.
- Validation: shard independence, pinned migration exclusion, failed-lease retry,
  reverse reclamation and stale-generation reuse; existing NVPool KVM tests.
- Performance: repeat NVMe random 4K/16 jobs, 64 vCPUs, 256M file range, 64M
  NVPool (16M metadata/48M data), with identical diagnostic and normal modes.
  Compare busy throughput and completion publication/reclaim timing. Completed:
  normal busy throughput 10.85 -> 24.95/25.00 MiB/s; instrumented per-page
  completion service 379.77 -> 158.15 us and mean reclaim lock acquisition
  33.225 -> 0.246 us. Five directed KVM tests and eight IRQ-context static tests
  pass; full-system and long-running concurrency matrices were not run.
- Evidence: `test/aster-ext4-baseline-test/results/20261011-pool-shards-validation/README.md`.
  Diagnostics are disabled in the final source and the final rebuilt/tested kernel.

### Inode-local publication (2026-10-12)

Current validation scope is performance only. Ordinary kernels exclude durable
image allocation and copying and reject model mode; crash-model fixtures remain
ktest-only for future validation. Backend-wide byte-access exclusion is removed
for ordinary modes using existing object pins, mutation leases and publication;
Per-NVPage runtime state and reverse now share a direct slot lock; the migration
loop combines identity/reverse validation into one acquisition and then reads
the descriptor outside the lock. Reverse is an inline 8-byte optional locator,
generation-checked against ObjectRecord. Runtime validation and fio comparison
are recorded in `20261012-per-slot-reverse/`.

### Deferred native extent checkpoint (2026-10-12)

- Remove per-page native extent/inode materialization from rNVPCe data completion;
  retain allocation/data durability before NV-to-SSD reference publication.
- Export SSD-selected NV extent entries at explicit filesystem sync/unmount,
  using inner-before-publication exclusion and bounded tree traversal without
  flatten/rebuild. Keep NV metadata resident and recoverable; metadata eviction
  and SSD-only detach are not implemented by this step.
- Verify read-after-migration with native mappings absent, explicit export and
  retry, and over-capacity fio; repeat the NVMe 4K/16-job bandwidth baseline.
- Native-only fallocate/unlink/rename replacement are rejected on rNVPCe until
  persistent NV graph reclamation is implemented; native EXT4 is unchanged.
- Validation: journal/nojournal KVM tests and eight static checks pass; three
  over-capacity fio rounds complete and unmount. Median busy 89.82MiB/s with
  significant variability; completion materialize/inner-wait counters are zero.
  Evidence: `test/aster-ext4-baseline-test/results/20261012-deferred-materialize/`.

### Allocation lock and combined reclaim (2026-10-12)

- [x] Move free_ids, capacity/watermarks and next_alloc_seq into AllocationState;
  update allocation, abort, legacy free, WAL accounting, stats and recovery.
- [x] New rNVPCe completion consumes its lease through reclaim_after_ssd_publish,
  clearing the descriptor before atomically returning slot/capacity; retain
  generation/lease checks and legacy generic reference checks.
- [x] Validate lock independence, failure/retry, parallel accounting, slot reuse,
  batched migration, capacity policy and over-capacity fio completion.
- Six KVM tests and eight static checks pass. Two fast fio runs complete at
  29.80/94.81 MiB/s; variability is unresolved, so no stable speedup is claimed.
- Evidence directory: `test/aster-ext4-baseline-test/results/20261012-allocation-reclaim/`.
- Lock order: pool control -> allocation -> one slot. Successful new-path reclaim
  omits pool control. The finalize timing now includes allocation-lock wait and
  slot/capacity return; the old separate free-page timing is not recorded there.
- Follow-up: retain the full fallible validation before descriptor clearing;
  finalize uses debug assertions while the same lease remains active, then
  unconditionally clears reverse and returns the slot. Validation evidence:
  `test/aster-ext4-baseline-test/results/20261012-reclaim-single-validation/`.
- Performance follow-up: uninstrumented fast median 76.42 MiB/s (three runs);
  fast repeat/no-persist reclaim 5.90/6.26 us/page. First fast profile had an
  unreproduced SSD durability stall; retain all samples. Evidence:
  `test/aster-ext4-baseline-test/results/20261012-merged-reclaim-profile/`.

- [x] Replace pool-control publication ownership and global wake-all with an
  inode-local owner mutex, wait queue and owned RAII publication guard.
- [x] Cache the shared object in native EXT4 Inode; pass checked publication
  ownership into foreground range writes and completion migration batches.
- [x] Keep a separate weak identity directory for initialization/recovery and
  standalone callers; retain identity while guards or waiters remain alive and
  remove expired entries.
- [x] Preserve inner-lock-before-publication ordering, the 128-page completion
  bound, persistent updates and reclamation semantics.
- [x] KVM tests: context lifetime/isolation, same-inode waiter exclusion, and
  batched leaf migration.
- [x] Repeat real NVMe random 4K/16-job fio with diagnostics disabled: no-persist
  38.04/33.68 MiB/s; default fast 17.85/24.15 MiB/s. All four points complete;
  comparison with preceding samples does not establish a stable bandwidth gain.

Evidence: `test/aster-ext4-baseline-test/results/20261012-inode-publication/`.

### Completion node exclusion (2026-10-13, implemented and tested)

- Checkpoint: kernel `644fd298d`, workloads `3b48118ae`, documents `8de5530`.
- Foreground retains inode publication for topology changes, and acquires a semantic
  metadata-node guard before reading each live node it may modify. It retains these
  guards through durable mutation and reverse-index repair.
- Completion acquires one leaf guard at a time, revalidates reverse/forward identity,
  and changes NV references to SSD without acquiring inode publication. Relocated
  entries are resolved again; failed pending mutations block the affected leaf.
- Existing reservation records have their own guard covering read-modify-persist;
  initial creation remains under foreground publication. Completion releases its leaf
  guard before acquiring reservation ownership or reclaiming data frames.
- Acceptance: KVM tests for same-node exclusion, independent-node progress,
  publication-independent completion, reservation accounting and mutation recovery;
  then real NVMe random-4K/16jobs/256MiB/64MiB NVPool bandwidth and latency comparison.
- Results: `test/aster-ext4-baseline-test/results/20261013-leaf-completion/`.
- Eight final KVM tests and nine static checks pass. Three uninstrumented fio runs
  finish256 MiB at98.46/94.46/109.40MiB/s; median98.46 vs98.84 before is unchanged.
  Two profiles confirm no inode publication acquisition; leaf lock admission is
  1.85-1.90us and reservation lock admission0.31-0.40us. Leaf processing remains
  expensive, so removing the coarse gate does not yet demonstrate a bandwidth gain.
- Concurrent pending-mark readers required two fixes: drain readers before phase
  update/clear, and atomically validate persistent identity with pin admission.
  Failed diagnostic runs are excluded; targeted regression covers late readers.

### Completion pending-index isolation (2026-10-13)

- [x] Remove completion pending lookup from `pool.state` using64 inode-hashed
  directory shards; preserve the existing leaf mutation guard and identity pinning.
- [x] Register candidates before persistence, compare-remove after durable clear,
  retain conservative state on ambiguous failure, and rebuild before recovery repair.
- [x] KVM: lookup while pool state is held; inode/shard isolation and generation reuse;
  existing rewrite/split recovery, pending-reader interleaving and batched migration.
- [x] Compare three uninstrumented same-session baseline/optimized fio samples and
  two optimized latency profiles. Pending-lock wait drops to0.20us and leaf service
  to19.02us, but busy median33.73MiB/s vs90.14 before shows no end-to-end gain.
  Retain low samples; do not infer speedup from the best run.
- Evidence: `test/aster-ext4-baseline-test/results/20261013-pending-index/`.

### Optional CPU isolation for performance validation (2026-10-13)

- Default-off `rtio_cpu_isolation=on`, minimum64vCPU: users0-31, default
  kernel threads32-59, flush60, completion61, virtio submitters62, journal63.
- Apply masks before spawn; do not rely on unsupported affinity migration or
  currently absent POSIX mask inheritance. Keep nice0 and durability unchanged.
- Verify entry CPUs with optional `rtio_cpu_trace=on`, then disable trace for
  throughput runs. IRQ and host CPU affinity are outside this experiment.
- Eight complete NVMe/random4K/16-job runs show98.08-108.94MiB/s, median98.46,
  with no30+MiB/s sample. This is a bounded mitigation result, not proof of
  universal scheduler correctness. Evidence: [CPU isolation](../test/aster-ext4-baseline-test/results/20261013-cpu-isolation/README.md).

### Completion leaf batching (2026-10-11)

- [x] Add regression coverage: two pages in one leaf produce one generation update;
  invalid forward identity changes no entries; retry consumes no extra reservation.
- [x] Regroup durable BIO completions by inode, sort by logical page, and bound each
  publication window to 128 pages; yield between windows.
- [x] Resolve current reverse locators under publication, group by leaf, validate all
  leaf entries before mutation, and persist one combined image per leaf per window.
- [x] Keep per-page migration-credit phases and combine reservation-ledger refresh;
  failed leaf groups retain NV residency while successful groups reclaim.
- [x] Finish KVM modeled-recovery and relocated-reverse regression tests.
- [x] Run the NVMe random-4K/16-job real-device comparison with identical diagnostics,
  retain counters and busy throughput, then disable diagnostics for normal builds.

Evidence: `test/aster-ext4-baseline-test/results/20261011_230050_aster-ext4-baseline/completion-leaf-batching-analysis.md`.
Instrumented busy throughput: 6.23 -> 10.41 MiB/s; completion service amortized per
page: 701.77 -> 379.77 microseconds. One run per implementation; diagnostics are
disabled for normal builds. Recovery uses modeled restart, not real power loss.
Diagnostics-disabled same-workload repeat: 10.85 MiB/s, 256 MiB completed,
`20261011_230249_aster-ext4-baseline`.
