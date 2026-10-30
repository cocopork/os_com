# Page Descriptor Redirection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rNVPCe's mixed SSD/NV extent residency with persistent per-page descriptor redirects plus a rebuildable DRAM contiguous-page-range index, so ordinary overwrites and migration completion do not read or modify extent leaves.

**Architecture:** The confirmed NVM extent tree remains the SSD baseline; the selected descriptor with the greatest logical version is the authority for a newer NVPage. A 64-shard `RedirectDirectory` provides exact PageKey lookup, pending version reservations, dirty contiguous-page-range selection, and exact-version removal, while a volatile baseline range-generation tracker detects true SSD mapping changes. Foreground writes, rTIO claim, and completion use descriptor handles and short shard critical sections; FAST/FAIR remains only for structural SSD baseline changes.

**Tech Stack:** Rust `no_std`, Asterinas kernel, rNVPCe descriptors and FAST/FAIR extent metadata, EXT4, OSDK/QEMU ktests, fio.

**Spec:** `docs/superpowers/specs/2026-10-27-page-descriptor-redirection-design.md`

## Global Constraints

- This change applies only to rNVPCe; the rNVPC WAL protocol is unchanged.
- A selected descriptor in `NvDirty`, `NvWriteback`, or `NvReclaimable` remains authoritative until its selector is durably cleared.
- `allocation_seq` prevents physical-slot ABA; `logical_version` orders values of one `PageKey`; equal selected versions are corruption and overflow rejects publication.
- Ordinary overwrite of an existing SSD baseline must not mutate, split, or republish the extent tree.
- No redirect shard lock may be held during NVM persistence, SSD allocation, BIO submission, device flush, or extent-tree mutation.
- Descriptor publication and clearing remain bounded to chunks of at most 128 pages and end with one selector fence per chunk.
- Truncate, hole punch, relocation, and other range-deletion operations remain on the existing serialized path until a persistent tombstone/range-epoch protocol exists.
- Use “contiguous page range” / “连续页区间”, `RedirectRange`, and “dirty page range” / “脏页区间`; do not introduce “run” for these concepts.
- Do not silently reinterpret an old persistent image; bump `NV_IMAGE_VERSION` before removing mixed NV extent entries, and reject the old image without a conversion tool.

## Review Focus

- A late completion for generation N must never remove a concurrently published generation N+1; Task 6 adds exact-handle completion tests.
- Two publishers for one PageKey must not reserve the same logical version while persistence occurs outside the shard lock; Task 2 adds interleaved reservation tests.
- A dirty range crossing a shard or batch boundary must split cleanly without dropping or duplicating pages; Task 5 adds boundary tests.
- Recovery must select by logical version rather than physical scan order, including `NvWriteback` and `NvReclaimable`; Task 1 adds reversed-slot and state-matrix tests.
- A baseline mutation during SSD I/O must force re-resolution instead of clearing redirects against stale targets; Task 6 adds generation-change tests.

---

## File Structure

- Create `kernel/src/fs/rnvpc/redirect_index.rs`: exact descriptor handles, 64-shard PageKey map, pending publication tokens, compatible `RedirectRange` coalescing, dirty-range queue, and exact compare/remove.
- Create `kernel/src/fs/rnvpc/baseline_generation.rs`: volatile page-range generation snapshots and overlap invalidation for confirmed SSD mappings.
- Create `kernel/src/fs/rnvpc/frame/redirect.rs`: pool-facing descriptor publish, redirect pin/claim, state transition, exact selector clear, and recovery rebuild operations.
- Modify `kernel/src/fs/rnvpc/index.rs`: retain `PageKey` and generic DRAM-page indexing; delete data-page reverse extent locators after all consumers move.
- Modify `kernel/src/fs/rnvpc/frame.rs`: own the new directories, expose exact object-generation pins, and remove extent-based migration identities and global dirty/reverse state.
- Modify `kernel/src/fs/rnvpc/recover.rs`: select one data descriptor per PageKey by logical version, rebuild redirects/ranges, and retire obsolete generations idempotently.
- Modify `kernel/src/fs/rnvpc/write_transaction.rs`: redirect-first lookup, descriptor-only ordinary overwrite, and structural-only baseline publication.
- Modify `kernel/src/fs/rnvpc/mod.rs`: expose range-oriented rNVPCe flush candidates and remove reverse-locator candidate discovery.
- Modify `kernel/src/fs/fs_impls/ext4/inode/mod.rs`: use redirect-first reads and choose ordinary-overwrite versus structural-write transactions.
- Modify `kernel/src/fs/fs_impls/ext4/migration.rs`: claim dirty ranges, submit their pages, and complete existing baselines without leaf routing/publication.
- Modify `kernel/src/fs/rnvpc/batch_metrics.rs` and EXT4 latency counters: count redirect lookups, range claims, descriptor batches, leaf fast-path violations, version comparisons, and contiguous slot reclamation.
- Modify `kernel/src/fs/rnvpc/{tests.rs,rnvpce_integrity_tests.rs,persistent_tests.rs}` and `kernel/src/fs/fs_impls/ext4/nv_tests.rs`: unit, crash-boundary, concurrency, and end-to-end regression coverage.
- Modify `kernel/src/fs/rnvpc/{metadata.rs,extent_target.rs,layout.rs}` only in the final cleanup task: reject/remove NV data targets and bump the image version.

### Task 1: Make Descriptor Recovery Select the Authoritative Logical Version

**Files:**
- Modify: `kernel/src/fs/rnvpc/recover.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/index.rs`
- Test: `kernel/src/fs/rnvpc/persistent_tests.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: `RecoveredEntry { index, key, version, allocation_seq, ssd_backing_block }` and selected `NvDescriptor` states.
- Produces: `RecoveredDataSet { selected: BTreeMap<PageKey, RecoveredEntry>, obsolete: Vec<RecoveredEntry> }` from `recover::select_data_versions(entries) -> Result<RecoveredDataSet>`; `PageEntry` records both `PersistentObjectRef` and `logical_version` until Task 2 replaces its data role.

- [ ] **Step 1: Write failing recovery tests**

  Add `rnvpce_recovery_selects_highest_data_version_independent_of_slot_order`, `rnvpce_recovery_rejects_equal_data_versions`, `rnvpce_recovery_keeps_all_redirect_states_authoritative`, and `rnvpce_recovery_cleanup_is_idempotent`. Assert that reversed physical slot order still selects the highest `recovery_id`, a tie returns `EUCLEAN`, all three NV lifecycle states rebuild, and a second recovery produces the same selected map and free-slot count.

- [ ] **Step 2: Run the focused tests and confirm the old physical-order insertion fails**

  Run: `cargo osdk test rnvpce_recovery --package aster-kernel -- --nocapture`

  Expected: at least the reversed-order or equal-version test fails before implementation.

- [ ] **Step 3: Implement deterministic data selection before pool exposure**

  Add `select_data_versions`, separate selected and obsolete data entries in `scan`/`Rnvpc::recover`, and retain exact `(slot, allocation_seq, logical_version)` in the temporary runtime entry. Reject equal maxima with `EUCLEAN`; do not use extent reachability to discard the selected redirect.

- [ ] **Step 4: Reconcile obsolete generations only after selected descriptors are installed**

  Update `recover::reconcile` so selected data descriptors are live independently of NV extent entries, while lower versions are reclaimed only after authoritative lookup and pin state are rebuilt. Keep legacy extent traversal as a cross-check during this staged task.

- [ ] **Step 5: Run focused recovery tests and ktest typecheck**

  Run: `cargo osdk test rnvpce_recovery --package aster-kernel -- --nocapture`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Expected: all focused tests pass and ktest compilation exits 0.

- [ ] **Step 6: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/recover.rs kernel/src/fs/rnvpc/frame.rs kernel/src/fs/rnvpc/index.rs kernel/src/fs/rnvpc/persistent_tests.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
  git commit -m "fix(rnvpce): select authoritative descriptor versions"
  ```

### Task 2: Add the Sharded Redirect Directory and Pending Version Reservations

**Files:**
- Create: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Test: `kernel/src/fs/rnvpc/redirect_index.rs`

**Interfaces:**
- Consumes: `PageKey`, `PersistentObjectRef`, `NvObjectState`.
- Produces: `DescriptorHandle { reference, logical_version, state, ssd_target }`; `RedirectRange { fs_id, inode, logical_start, pages: Vec<DescriptorHandle>, epoch }`; `PendingRedirectBatch`; and `RedirectDirectory::{lookup, reserve_versions, commit_reserved, abort_reserved, rebuild, dirty_ranges, compare_remove}`.

- [ ] **Step 1: Write failing in-module tests for exact handles, reservations, and coalescing**

  Add tests named `redirect_reservations_are_monotonic_while_persist_is_unlocked`, `redirect_ranges_merge_only_compatible_contiguous_pages`, `redirect_partial_overwrite_splits_only_dram_ranges`, `redirect_exact_remove_preserves_newer_generation`, and `redirect_dirty_range_splits_at_shard_and_budget_boundaries`. Assert 32 adjacent logical pages with adjacent slots and compatible state form one range inside a shard, allocation generations remain explicit per page, discontinuities split, and every page appears exactly once across returned snapshots.

- [ ] **Step 2: Run the new module filter and confirm it fails to compile**

  Run: `cargo osdk test redirect_ --package aster-kernel -- --nocapture`

  Expected: FAIL because `RedirectDirectory` and its methods do not exist.

- [ ] **Step 3: Implement `redirect_index.rs` with 64 short-held shards**

  Store shards as `Box<[SpinLock<RedirectShard>]>`; map by owner plus `page_index / 256`, and acquire multiple shards in increasing shard-number order. `reserve_versions(&[PageKey])` installs pending values before releasing locks; `commit_reserved` accepts only the exact token/version pairs; `abort_reserved` removes only that token. Neither method performs persistence.

- [ ] **Step 4: Implement volatile range coalescing and dirty discovery**

  Coalesce only adjacent PageKeys and adjacent NV slots with compatible lifecycle state and SSD target continuity. Keep every page's exact `PersistentObjectRef` and logical version. A stale page or budget boundary yields valid shorter ranges rather than rejecting unrelated pages.

- [ ] **Step 5: Add `redirects: RedirectDirectory` to `Rnvpc` and rebuild it from Task 1's selected set**

  Keep the legacy `PageIndex`, dirty queue, and reverse locators temporarily for cross-check assertions. Add ktest-only equality checks between old and new lookup results; production reads still use the old path until Task 4.

- [ ] **Step 6: Run index tests and typecheck**

  Run: `cargo osdk test redirect_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Expected: redirect tests pass and typecheck exits 0.

- [ ] **Step 7: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/redirect_index.rs kernel/src/fs/rnvpc/mod.rs kernel/src/fs/rnvpc/frame.rs
  git commit -m "feat(rnvpce): add sharded redirect range index"
  ```

### Task 3: Publish Descriptor Batches Through the Redirect Lifecycle

**Files:**
- Create: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/frame/migration.rs`
- Modify: `kernel/src/fs/rnvpc/persist.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: `RedirectDirectory::{reserve_versions, commit_reserved, abort_reserved, compare_remove}`.
- Produces: `Rnvpc::publish_redirect_batch(pages, fault) -> Result<Vec<DescriptorHandle>>`; `Rnvpc::pin_redirect(key, expected) -> Result<PinnedDataPage>`; `Rnvpc::transition_redirects(handles, state, fault) -> Result<Vec<DescriptorHandle>>`; `Rnvpc::clear_redirects_exact(handles, fault) -> Result<Vec<NvPageId>>`.

- [ ] **Step 1: Write failing crash and concurrency tests**

  Add `rnvpce_redirect_publish_survives_every_selector_boundary`, `rnvpce_concurrent_publishers_get_distinct_logical_versions`, `rnvpce_logical_version_overflow_rejects_publication`, `rnvpce_slot_reuse_rejects_old_allocation_generation`, `rnvpce_failed_publish_aborts_only_its_pending_token`, `rnvpce_uncertain_selector_latches_recovery_required`, and `rnvpce_clearing_old_descriptor_keeps_new_redirect`.

- [ ] **Step 2: Run the focused lifecycle tests and verify failure**

  Run: `cargo osdk test rnvpce_redirect_ --package aster-kernel -- --nocapture`

  Expected: FAIL because batch lifecycle APIs do not exist.

- [ ] **Step 3: Implement bounded descriptor publication**

  Reserve versions under redirect shards, release the shards, persist data and inactive descriptor copies, publish selectors in chunks of 128, then commit the exact token into the redirect directory. On pre-selector failure abort and free uncommitted slots; on uncertain selector durability set the existing recovery-required latch.

- [ ] **Step 4: Implement exact pins, transitions, and selector clearing**

  Recheck `(PageKey, reference, logical_version)` after obtaining the object-generation pin without rereading the descriptor. Clear selectors first, then `compare_remove` the exact old handles; if a newer handle exists, retire only the old slot. Return reclaimable slots grouped later by physical adjacency.

- [ ] **Step 5: Replace unsafe key-only removal in `clear_committed`**

  Ensure clearing an old data descriptor cannot call unconditional `state.index.remove(key)`. During the staged dual-index period, remove the legacy entry only if its stored reference and logical version match the cleared descriptor.

- [ ] **Step 6: Run lifecycle tests and the existing persistent transaction suite**

  Run: `cargo osdk test rnvpce_redirect_ --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpc_persistent --package aster-kernel -- --nocapture`

  Expected: all selected tests pass.

- [ ] **Step 7: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/frame/redirect.rs kernel/src/fs/rnvpc/frame.rs kernel/src/fs/rnvpc/frame/migration.rs kernel/src/fs/rnvpc/persist.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
  git commit -m "feat(rnvpce): publish exact descriptor redirects"
  ```

### Task 4: Switch Reads and Ordinary Overwrites to Redirect-First Lookup

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `Rnvpc::{publish_redirect_batch, pin_redirect}` and `RedirectDirectory::lookup`.
- Produces: `mapped_nv_page_ref` becomes redirect-only; `commit_existing_baseline_redirect_write(...) -> Result<CommittedRangeWrite>`; structural writes continue through the existing `commit_range_write_with_mutation` slow path.

- [ ] **Step 1: Write failing read/write behavior tests**

  Add `rnvpce_middle_page_overwrite_keeps_extent_tree_bytes_unchanged`, `rnvpce_reads_mixed_ssd_and_redirect_pages`, `rnvpce_128k_overwrite_publishes_one_redirect_range`, and `rnvpce_write_during_old_writeback_publishes_cow_generation`. Snapshot the root/leaf bytes and entry count before a `[4,5)` overwrite of SSD `[0,8)`, then assert byte-for-byte equality afterward and correct reads for all eight pages.

- [ ] **Step 2: Run the focused tests and confirm the extent-mutation assertion fails**

  Run: `cargo osdk test rnvpce_redirect_io --package aster-kernel -- --nocapture`

  Expected: the current path splits/publishes an NV extent entry or lacks redirect-first lookup.

- [ ] **Step 3: Make read lookup redirect-first**

  Change `mapped_nv_page_ref` and `read_mapped_page_from` to obtain the exact runtime handle, pin its allocation generation, recheck identity in memory, and read NV data. A redirect miss alone falls back to the confirmed SSD extent tree in `Ext4Inode::read_rnvpce_at`.

- [ ] **Step 4: Add the ordinary existing-baseline write path**

  For every page whose current SSD baseline already maps the supplied target, load partial-page data from the current redirect or SSD baseline, allocate contiguous NV slots when available, and call `publish_redirect_batch`. Publish inode size/generation with the same extent root; do not call `replace_tree_extents`, install reverse locators, or construct NV `ExtentRecordEntry` values.

- [ ] **Step 5: Preserve the structural slow-path boundary**

  Route holes, new allocation, unwritten conversion, truncate, hole punch, and relocation through the existing serialized transaction. If an old redirect is in writeback, publish a new descriptor generation rather than waiting for its extent or inode.

- [ ] **Step 6: Run redirect I/O and complete rNVPCe integrity tests**

  Run: `cargo osdk test rnvpce_redirect_io --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_ --package aster-kernel -- --nocapture`

  Expected: new and existing tests pass; ordinary-overwrite leaf bytes remain unchanged.

- [ ] **Step 7: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/write_transaction.rs kernel/src/fs/rnvpc/mod.rs kernel/src/fs/fs_impls/ext4/inode/mod.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs kernel/src/fs/fs_impls/ext4/nv_tests.rs
  git commit -m "feat(rnvpce): redirect ordinary overwrites by descriptor"
  ```

### Task 5: Replace Extent-Based Candidate Claim With Dirty Contiguous-Page Ranges

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/tests.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: `RedirectDirectory::dirty_ranges` and exact descriptor handles.
- Produces: `ClaimedRedirectRange { identity, baseline_token, pages: Vec<ClaimedRedirectPage> }`; `Rnvpc::claim_redirect_range(snapshot, budget) -> Result<Vec<ClaimedRedirectRange>>`; `rnvpce_flush_candidates` returns range snapshots instead of `RnvpceFlushCandidate` with `ReverseExtentRef`.

- [ ] **Step 1: Write failing range-claim tests**

  Add `rnvpce_claims_32_pages_with_one_range_snapshot`, `rnvpce_claim_splits_around_one_stale_page`, `rnvpce_claim_obeys_budget_without_losing_tail`, and `rnvpce_claim_reads_no_descriptor_or_extent_leaf`. Assert exact page order/versions, retry visibility for unclaimed tails, and zero persistent descriptor reads, leaf reads, reverse lookups, and inode publication-guard acquisitions.

- [ ] **Step 2: Run candidate tests and verify the current extent claim violates counters**

  Run: `cargo osdk test rnvpce_redirect_claim --package aster-kernel -- --nocapture`

  Expected: FAIL because current candidate grouping routes a leaf and validates every reverse edge.

- [ ] **Step 3: Implement one in-memory claim per candidate range**

  Snapshot exact handles under the relevant shards, pin their allocation generations, transition accepted pages to `NvWriteback` in bounded descriptor batches, and release shards before target allocation or I/O. Split on stale pages and batch limits; requeue valid unclaimed suffixes.

- [ ] **Step 4: Rewire `migrate_nv_round` preparation**

  Delete grouping by `(inode, leaf_slot)` and `claim_migration_extents`. Build `PendingPage` directly from claimed descriptor handles and their SSD targets. Existing-baseline candidates must not obtain `MigrationExtentIdentity` or read an extent node.

- [ ] **Step 5: Run claim tests and rNVPCe integrity tests**

  Run: `cargo osdk test rnvpce_redirect_claim --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_ --package aster-kernel -- --nocapture`

  Expected: tests pass with zero candidate-path persistent descriptor and leaf reads.

- [ ] **Step 6: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/redirect_index.rs kernel/src/fs/rnvpc/frame/redirect.rs kernel/src/fs/rnvpc/mod.rs kernel/src/fs/fs_impls/ext4/migration.rs kernel/src/fs/rnvpc/tests.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs
  git commit -m "feat(rnvpce): claim dirty redirect ranges"
  ```

### Task 6: Complete Existing-Baseline Migration Without Leaf Publication

**Files:**
- Create: `kernel/src/fs/rnvpc/baseline_generation.rs`
- Modify: `kernel/src/fs/rnvpc/mod.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: `ClaimedRedirectRange` and `Rnvpc::clear_redirects_exact`.
- Produces: `BaselineRangeToken { fs_id, inode, logical_start, page_generations }`; `BaselineGenerationMap::{snapshot, validate, advance}`; `finish_existing_baseline_ranges(pool, ranges, fault) -> Result<CompletionResult>`.

- [ ] **Step 1: Write failing completion and generation tests**

  Add `rnvpce_existing_baseline_completion_reads_and_publishes_no_leaf`, `rnvpce_old_completion_preserves_newer_redirect`, `rnvpce_older_ssd_bio_cannot_overwrite_newer_submission`, `rnvpce_baseline_change_forces_slow_reresolution`, `rnvpce_completion_clears_selectors_in_128_page_chunks`, and `rnvpce_completion_reclaims_contiguous_slot_ranges`. Assert leaf read/write counters stay zero on the fast path, a new redirect remains readable, overlapping target writes retain PageKey migration ownership until the older flush completes, an overlapping generation advance prevents clearing, and each reclaimed slot appears exactly once.

- [ ] **Step 2: Run focused completion tests and confirm current leaf publication fails them**

  Run: `cargo osdk test rnvpce_redirect_completion --package aster-kernel -- --nocapture`

  Expected: FAIL because `publish_claimed_nv_extent_to_ssd` rewrites leaf targets.

- [ ] **Step 3: Implement page-range baseline generations**

  Use a volatile sharded map with generation zero as the implicit initial value. `snapshot(range)` returns the exact per-page generation vector; `advance(range)` increments only intersecting logical pages with overflow rejection; `validate(token)` compares the whole vector. Advance only after a true confirmed NVM baseline mutation commits.

- [ ] **Step 4: Implement existing-baseline fast completion**

  After all data/metadata BIOs and the batch device flush complete, validate the baseline token and compare current redirect handles to submitted handles. For exact matches, persist credit/ledger state in batch, clear selectors in chunks of 128, remove only exact handles, then return contiguous physical slot ranges. Do not route the inode tree, read a leaf, publish an extent target, or take an inode publication guard.

- [ ] **Step 5: Implement changed-baseline slow handling**

  On token mismatch, resolve only the affected current SSD baseline range. Clear a submitted redirect only if the current baseline still maps its exact migration target; otherwise restore it to `NvDirty` and requeue it. A newer redirect always remains authoritative.

- [ ] **Step 6: Keep structural completion ordering unchanged and explicit**

  New/hole/unwritten targets continue through FAST/FAIR baseline publication, then baseline-generation advance, credit release, descriptor clear, and reclaim in that order. Add assertions that the common existing-baseline entry point rejects structural candidates.

- [ ] **Step 7: Add structural-operation crash regression tests**

  Add `rnvpce_hole_write_crash_preserves_one_authority`, `rnvpce_delalloc_baseline_publish_crash_preserves_one_authority`, `rnvpce_unwritten_conversion_crash_preserves_one_authority`, and retained-serialized-path tests for truncate and hole punch. Inject faults before/after migration-target persistence, device-flush acknowledgement, baseline publication, credit release, and redirect clearing; assert recovery exposes either the selected redirect or a confirmed SSD baseline and never a dangling mapping.

- [ ] **Step 8: Run completion, crash, and full rNVPCe tests**

  Run: `cargo osdk test rnvpce_redirect_completion --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_batch_migration_crash --package aster-kernel -- --nocapture`

  Run: `cargo osdk test rnvpce_ --package aster-kernel -- --nocapture`

  Expected: all selected tests pass; existing-baseline counters report zero leaf reads/publications.

- [ ] **Step 9: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/baseline_generation.rs kernel/src/fs/rnvpc/mod.rs kernel/src/fs/rnvpc/frame.rs kernel/src/fs/rnvpc/frame/redirect.rs kernel/src/fs/rnvpc/write_transaction.rs kernel/src/fs/fs_impls/ext4/migration.rs kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs kernel/src/fs/fs_impls/ext4/nv_tests.rs
  git commit -m "feat(rnvpce): reclaim redirects without leaf publication"
  ```

### Task 6A: Publish First-Write SSD Baselines in the Foreground

**Status:** Implemented and covered by `ext4_research_routes` on 2026-10-29.

**Interfaces:**
- `Ext4::allocate_rnvpce_foreground_blocks(count)` allocates contiguous ranges
  under allocation-group ownership and publishes the resulting bitmap/GDT
  image to NVM without SSD I/O.
- `RangeWritePage::assign_new_ssd_block(target)` binds each new logical page to
  its foreground allocation before `commit_range_write_with_mutation`.
- `RangeWriteBatch.allocations` persists allocation ownership; its coalesced
  replacements publish the SSD baseline in the same foreground transaction as
  the redirect and inode state.

- [x] Add a failing EXT4 route test proving that a first write previously
  returned with no SSD baseline.
- [x] Allocate and persist SSD targets before foreground redirect publication;
  roll back every private range if allocation or bitmap publication fails.
- [x] Publish one baseline extent per contiguous allocation range while keeping
  redirect-first reads authoritative until device flush and exact clear.
- [x] Prove migration/completion performs zero structural baseline mutations
  for a new foreground write.
- [x] Update fragmented-allocation, failed-flush, and allocation-durability
  scenarios for the new responsibility boundary.

The target-allocation branch in migration remains only for recovery of legacy
descriptors with `ssd_target == 0`; normal foreground writes cannot create one.

### Task 7: Remove Data Reverse Locators and Mixed NV Extent Residency

**Files:**
- Modify: `kernel/src/fs/rnvpc/index.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/recover.rs`
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/metadata.rs`
- Modify: `kernel/src/fs/rnvpc/extent_target.rs`
- Modify: `kernel/src/fs/rnvpc/layout.rs`
- Modify: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`
- Test: `kernel/src/fs/rnvpc/persistent_tests.rs`

**Interfaces:**
- Consumes: all redirect-first read/write/claim/completion interfaces from Tasks 2-6.
- Produces: SSD-only leaf `ExtentRecordEntry` data targets; no `ReverseExtentRef`, `MigrationExtentIdentity`, `claim_migration_extents`, NV extent coalescing, or data reverse maintenance remains in production.

- [ ] **Step 1: Add failing format and obsolete-path tests**

  Add `rnvpce_new_image_rejects_nv_extent_data_target`, `rnvpce_old_image_version_is_rejected`, and `rnvpce_recovery_rebuilds_redirects_without_reverse_edges`. Assert SSD and child-route entries remain valid, NV data targets fail decoding/creation, and a descriptor-only image recovers readable data.

- [ ] **Step 2: Run the tests and confirm mixed NV entries are still accepted**

  Run: `cargo osdk test rnvpce_new_image --package aster-kernel -- --nocapture`

  Expected: FAIL because image version 4 and `ExtentDataRef::Nv` remain accepted.

- [ ] **Step 3: Bump the persistent image version and remove normal NV-entry construction**

  Increment `NV_IMAGE_VERSION` from 4 to 5. Make leaf data entries represent SSD written/unwritten mappings only; preserve child routes and FAST/FAIR structural updates. Reject version-4 images without conversion.

- [ ] **Step 4: Delete legacy reverse/extent migration state and dual-index cross-checking**

  Remove `ReverseExtentRef`, `ObjectRecord::reverse`, physical dirty membership keyed only by slots, extent-based claim identities, old NV replacement/coalescing helpers, and recovery reachability rules that require data descriptors to appear in an extent leaf. Retain generic DRAM PageCache indexing only where rNVPC still uses it.

- [ ] **Step 5: Run all rNVPC/rNVPCe tests and ktest typecheck**

  Run: `cargo osdk test rnvpc --package aster-kernel -- --nocapture`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Expected: all selected tests pass and ktest compilation exits 0.

- [ ] **Step 6: Commit**

  ```bash
  git add kernel/src/fs/rnvpc
  git commit -m "refactor(rnvpce): remove mixed NV extent residency"
  ```

### Task 8: Add Diagnostics, Run Full Correctness Validation, and Measure Performance

**Files:**
- Modify: `kernel/src/fs/rnvpc/batch_metrics.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/mod.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `docs/experiments/scripts/` scripts selected from the existing rNVPCe pipeline harness
- Create: `docs/experiments/reports/PAGE-DESCRIPTOR-REDIRECTION-2026-10-27.md`

**Interfaces:**
- Consumes: final redirect write/claim/completion paths.
- Produces: counters/timers for descriptor batches, persisted bytes/fences, redirect lookup/shard wait, candidate ranges/pages, candidate descriptor/leaf reads, completion comparisons/clear batches/reclaimed slot ranges, and structural baseline mutations.

- [ ] **Step 1: Add counter assertions to focused ktests**

  Extend the 128 KiB ordinary-overwrite/migration test to assert one coalesced redirect range where shard boundaries permit, zero candidate descriptor reads, zero existing-baseline leaf reads/publications, bounded selector batches, and 32 reclaimed pages.

- [ ] **Step 2: Add and wire phase diagnostics**

  Record foreground descriptor batch count/bytes/fences, redirect lookup and shard-lock nanoseconds, candidate range/page count and claim nanoseconds, persistent descriptor/leaf reads, completion comparisons/clear batches, contiguous slot-range reclamation, and structural baseline mutations. Reset and print them through the existing diagnostic mechanism.

- [ ] **Step 3: Run formatting, diff validation, typechecks, and full kernel tests**

  Run: `cargo fmt --all -- --check`

  Run: `git diff --check`

  Run: `cargo osdk check --package aster-kernel`

  Run: `cargo osdk check --ktests --package aster-kernel`

  Run: `CONSOLE=ttyS0 SMP=1 MEM=4G cargo osdk test --package aster-kernel -- --nocapture`

  Expected: every command exits 0; record the exact passed/failed/filtered counts rather than inferring coverage from compilation.

- [ ] **Step 4: Run crash-boundary coverage**

  Execute the descriptor data/copy/selector fault matrix and migration target/device-flush/baseline/credit/clear matrix, including repeated recovery. Expected: every durable prefix yields either the old redirect/SSD baseline or the newest selected descriptor, never a dangling reference.

- [ ] **Step 5: Run the established fio matrix on an authorized disposable NVMe partition**

  Use the existing Docker/QEMU isolation harness with 2 GiB workload, 64 MiB dirty high watermark, 32 MiB low watermark, and direct device passthrough. Measure 4 KiB random write and 128 KiB sequential write at 1 and 16 threads; run sync, unmount, and `e2fsck` after each filesystem image. Prefer `/dev/nvme1n1p1`; use `/dev/nvme1n1p2` if the first partition is occupied, after verifying the selected partition is not mounted.

- [ ] **Step 6: Write the report with correctness and phase tables**

  Record commands, commit, device, QEMU CPU isolation, fio parameters, bandwidth/IOPS, foreground and rTIO phase totals/counts/averages, zero-read assertions, filesystem-check results, and comparison against the last repair-image and pre-refactor FAST/FAIR reports. Keep raw logs/results outside Git according to `docs/experiments` policy.

- [ ] **Step 7: Commit**

  ```bash
  git add kernel/src/fs/rnvpc/batch_metrics.rs kernel/src/fs/fs_impls/ext4 docs/experiments/scripts docs/experiments/reports/PAGE-DESCRIPTOR-REDIRECTION-2026-10-27.md
  git commit -m "test(rnvpce): validate page descriptor redirection"
  ```
