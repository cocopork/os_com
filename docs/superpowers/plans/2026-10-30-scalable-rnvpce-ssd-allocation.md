# Scalable rNVPCe SSD Allocation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rNVPCe first-write SSD allocation scale across CPUs by using per-CPU group selection, existing group-local free-space indexes, group-local ownership, and incremental NVM bitmap persistence.

**Architecture:** `AllocationGroupSelector` reads atomic group summaries and normally searches one CPU stripe. `BlockGroup::allocation_gate` owns migration/writeback exclusion per group, while `FreeSpaceIndex` remains the authoritative DRAM search structure. Resident NVM bitmap chunks keep stable identities and update only touched aligned words before SSD targets become visible.

**Tech Stack:** Rust 2024, Asterinas kernel/OSTD synchronization, EXT4 group allocator, rNVPC persistent metadata, OSDK ktests, QEMU/KVM, fio, e2fsck.

**Spec:** `docs/superpowers/specs/2026-10-30-scalable-rnvpce-ssd-allocation-design.md`

## Global Constraints

- Kernel code remains safe Rust; no `unsafe` is added under `kernel/`.
- No SSD I/O, device flush, wait, or sleep occurs under a block-group allocation lock.
- No operation holds two block-group allocation gates simultaneously.
- Bitmap bits are authoritative; free counts, checksums, and selection summaries are derived.
- An SSD target is published only after its NVM allocation bits are durable.
- inode/locality-group preallocation remains out of scope.
- Native EXT4 and rNVPC WAL allocation behavior must not change.
- Existing dirty worktree changes are preserved; commits stage only files owned by each task.

## Review Focus

- Near-full filesystems: fallback selection must find remote free groups without normal-path full scans or false `ENOSPC`.
- Thread migration: changing CPU between calls may change locality but cannot lose cursor state or allocation ownership.
- Multiword allocation crash cuts: partial durable allocation is a leak, never a duplicate allocation or published dangling target.
- Pager redirty race: an old writeback completion must retain newer NVM authority and resident chunks.
- Recovery with stale derived fields: free count, checksum, capacity, and group summary must be rebuilt from bitmap words.

---

### Task 1: Group Summary and Per-CPU Selector

**Files:**
- Create: `kernel/src/fs/fs_impls/ext4/allocator/group_selector.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/allocator/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/allocator/free_space.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Test: `kernel/src/fs/fs_impls/ext4/allocator/group_selector.rs`

**Interfaces:**
- Produces: `AllocationGroupSummary::{new,update,can_fit}`, `AllocationGroupSelector::new(nr_groups, nr_cpus)`, and `AllocationGroupSelector::candidates(cpu, goal_group, count) -> impl Iterator<Item = usize>`.
- Produces: `FreeSpaceIndex::longest_free_run() -> u32` and `BlockGroup::refresh_allocation_summary()`.
- Consumes: `ostd::cpu::{CpuId, num_cpus}` and the existing group-local `FreeSpaceIndex`.

- [x] **Step 1: Write failing selector and summary tests**

  Add `rnvpce_selector_uses_disjoint_cpu_stripes`, `rnvpce_selector_prefers_goal_then_last_success`, `rnvpce_selector_steals_after_home_exhaustion`, `rnvpce_selector_ignores_busy_group_without_enospc`, and `rnvpce_summary_tracks_allocate_free_and_recovery`. Assert exact candidate order for 8 groups/2 CPUs and summary values after bitmap mutations.

- [x] **Step 2: Run the focused tests and verify the API is absent**

  Run: `cd kernel && cargo osdk test rnvpce_selector_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: FAIL because `group_selector` and summary APIs do not exist.

- [x] **Step 3: Implement the atomic summary and selector**

  Store packed free-count/longest-run summaries with release/acquire ordering. Keep one atomic cursor and last-success group per selector lane plus one rotating steal cursor. Use `lane = cpu % min(nr_cpus, nr_groups)`; the home pass visits only `lane + k * min(nr_cpus, nr_groups)`. De-duplicate goal/last-success/home candidates. The fallback visits summaries from the steal cursor without locking group metadata.

- [x] **Step 4: Publish summaries from every bitmap mutation and recovery overlay**

  Update summaries while the group metadata lock still owns the matching `FreeSpaceIndex`. Add assertions that all alloc/free/recovery paths publish after index refresh.

- [x] **Step 5: Run focused and existing allocator tests**

  Run: `cd kernel && cargo osdk test rnvpce_selector_ --qemu-args="-enable-kvm -cpu host -display none"`

  Run: `cd kernel && cargo osdk test ext4_free_space_index --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: all selected tests pass.

### Task 2: Replace the Global Migration-owner Set with Group Leases

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/batch.rs`
- Test: `kernel/src/fs/fs_impls/ext4/migration.rs`

**Interfaces:**
- Consumes: selector candidates from Task 1.
- Produces: `AllocationGroupLease`, `BlockGroup::try_claim_migration() -> Result<AllocationGroupLease<'_>>`, and generation-checked lease release.
- Removes: `MigrationGroupOwners`, `MigrationGroupReservation`, and `Ext4::nv_migration_groups`.

- [x] **Step 1: Write failing group-lease concurrency tests**

  Add `rnvpce_disjoint_group_leases_progress_concurrently`, `rnvpce_same_group_has_one_migration_owner`, `rnvpce_stale_lease_cannot_release_new_owner`, `rnvpce_writeback_defers_owned_group`, and `rnvpce_busy_goal_retries_another_candidate`.

- [x] **Step 2: Run tests and verify the current global owner behavior fails the new contract**

  Run: `cd kernel && cargo osdk test rnvpce_group_lease_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: FAIL because ownership is filesystem-global rather than a generation-tagged group lease.

- [x] **Step 3: Extend `AllocationWritebackState` and implement RAII leases**

  Add `data_migration_owner: Option<u64>` and `next_owner_generation: u64`. A lease owns one group and generation; drop/release clears only the matching generation. rTIO retains it from target preparation through data completion, durability flush, and completion-state publication. Foreground allocation never retains a lease; it rejects owned/writeback groups, keeps mutation short, and marks `dirty`.

- [x] **Step 4: Route foreground and rTIO allocation through selector candidates**

  Try the exact goal first, then per-CPU candidates. Acquire and mutate one group at a time. Treat group ownership conflicts as retryable and continue selection; reserve `ENOSPC` for capacity or exhaustive summary failure.

- [x] **Step 5: Remove global owner state and run migration tests**

  Run: `cd kernel && cargo osdk test rnvpce_group_lease_ --qemu-args="-enable-kvm -cpu host -display none"`

  Run: `cd kernel && cargo osdk test rnvpce_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: selected tests pass and no production reference to `nv_migration_groups` remains.

### Task 3: Persist Stable NVM Bitmap Words In Place

**Files:**
- Modify: `kernel/src/fs/rnvpc/allocation_metadata.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/backend.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Produces: `AllocationMetadataCache::persist_allocated_range(group, range)` and `persist_freed_range(group, range)`.
- Produces: `Rnvpc::persist_allocation_bitmap_words(reference, byte_range, words)`, which validates object identity, writes only the touched chunk range, persists it, and contributes to one caller-owned fence.
- Changes: resident directory free count/checksum become snapshot-derived; `selected_image()` derives them from bitmap chunks.
- Changes: mutable resident chunks use `checksum == 0`; legacy nonzero checksums remain validated and are converted on admission.

- [x] **Step 1: Write failing incremental-persistence and crash-cut tests**

  Add `rnvpce_allocation_updates_only_touched_bitmap_words`, `rnvpce_multiword_partial_allocation_is_only_a_leak`, `rnvpce_live_partial_persist_quarantines_range`, `rnvpce_free_persist_failure_keeps_range_allocated`, `rnvpce_free_clears_bits_only_after_reference_removal`, `rnvpce_recovery_derives_count_from_bitmap`, `rnvpce_recovery_rejects_clear_bit_for_published_target`, `rnvpce_legacy_chunk_converts_on_admission`, and `rnvpce_stable_bitmap_refs_survive_repeated_allocations`.

- [x] **Step 2: Run tests and confirm full-image replacement violates expectations**

  Run: `cd kernel && cargo osdk test rnvpce_allocation_bitmap_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: FAIL because every update currently replaces complete chunk/directory objects.

- [x] **Step 3: Add the specialized range-persist primitive**

  Validate stable object kind, fs/group/chunk identity, mutable-format marker, range bounds, and 8-byte alignment. Hold the object mutation exclusion only for the range write and persist operation; do not allocate a repair image or replace the object reference. Return an explicit outcome that distinguishes a failure known to precede mutation from one that may have partially persisted.

- [x] **Step 4: Convert resident allocation metadata to stable chunks**

  Publish mutable chunks only on admission, converting validated legacy chunks. For allocation, set and persist touched words before target publication. On uncertain failure, quarantine and charge the complete requested range without publishing a target. For free, require completed reference removal before clearing and persisting words; uncertain failure retains the bits and charge. Recompute free count and checksum when reading a selected image or creating a pager snapshot.

- [x] **Step 5: Run crash, recovery, and existing allocation-cache tests**

  Run: `cd kernel && cargo osdk test rnvpce_allocation_bitmap_ --qemu-args="-enable-kvm -cpu host -display none"`

  Run: `cd kernel && cargo osdk test rnvpce_recovery_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: all selected tests pass at every defined crash cut.

### Task 4: Make Pager Handoff Group-local and Redirty-safe

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/block_group.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/rnvpc/allocation_metadata.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: group lease state from Task 2 and stable bitmap snapshots from Task 3.
- Produces: generation-checked `begin_allocation_writeback`, `finish_allocation_writeback`, and `abort_allocation_writeback` transitions on one `BlockGroup`.

- [x] **Step 1: Write failing pager race tests**

  Add `rnvpce_pager_does_not_hold_group_during_io`, `rnvpce_foreground_redirties_inflight_snapshot`, `rnvpce_old_completion_keeps_new_generation_resident`, and `rnvpce_different_groups_allocate_during_writeback`.

- [x] **Step 2: Run tests and observe the current ownership coupling fail**

  Run: `cd kernel && cargo osdk test rnvpce_pager_group_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: FAIL because pager ownership still depends on the filesystem-global group set.

- [x] **Step 3: Implement immutable snapshot submission outside locks**

  Copy bitmap/GDT after-images and arm the writeback generation under the group gate, release it, then perform SSD writes and flush. Completion reacquires only that group and transfers authority only when generation matches and `dirty` is clear.

- [x] **Step 4: Run pager and complete rNVPCe tests**

  Run: `cd kernel && cargo osdk test rnvpce_pager_group_ --qemu-args="-enable-kvm -cpu host -display none"`

  Run: `cd kernel && cargo osdk test rnvpce_ --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: all selected tests pass.

### Task 5: End-to-end Correctness and Performance Validation

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/pipeline_profile.rs`
- Create: `docs/experiments/scripts/scalable-rnvpce-allocation-20261030/`
- Create: `docs/experiments/reports/SCALABLE-RNVPCE-ALLOCATION-2026-10-30.md`

**Interfaces:**
- Consumes: Tasks 1-4 complete implementation.
- Produces: release throughput, diagnostic stage data, correctness evidence, and a before/after report.

- [x] **Step 1: Add disabled-by-default selector and persistence metrics**

  Measure home candidates, steal candidates, group-busy retries, summary false positives, group-lock wait, touched bitmap words, and bitmap persistence calls. Keep all compile-time instrumentation disabled in throughput images.

- [x] **Step 2: Run static and kernel verification**

  Run: `cargo fmt --all -- --check`

  Run: `cd kernel && cargo osdk test rnvpce_ --qemu-args="-enable-kvm -cpu host -display none"`

  Run: `cd kernel && cargo osdk test ext4_research_routes --qemu-args="-enable-kvm -cpu host -display none"`

  Expected: formatting and selected kernel suites pass with zero failures.

- [x] **Step 3: Run real-device correctness and throughput tests**

  On the already authorized NVMe test partition, reformat per point and run 2 GiB buffered sequential first-write: 4 KiB at jobs 1/2/4/8/16 and 128 KiB at jobs 1/16. Require fio status 0, sync/unmount success, and `e2fsck -fn` after every point.

- [x] **Step 4: Run a separate diagnostic sample**

  Enable only the new allocator metrics for 4 KiB jobs 1/16. Confirm normal allocation visits one home group on average, full-group steal is rare before pressure, and NVM bitmap persistence scales with touched words rather than 4 KiB images.

- [x] **Step 5: Record evidence and residual bottlenecks**

  Report fio/drain bandwidth, mean/P99 latency, selector visits, group-lock contention, NVM bytes persisted, allocation retries, and filesystem-check results. Do not attribute improvement without the uninstrumented A/B.
