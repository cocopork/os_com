# rNVPCe Open Redirect Extents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Amortize persistent redirect and inode publication across adjacent 4 KiB overwrite system calls without weakening write-return crash recovery.

**Architecture:** Keep one persistent `NvRedirectExtentRecord` as the recovery authority and one DRAM `DirtyNvExtentEntry` as its open runtime object. A compatible adjacent write persists its data first, then atomically advances the selected record copy's aligned 8-byte header containing `page_count`; pure overwrites whose inode root, size, and block count do not change reuse the current inode generation and inode record.

**Tech Stack:** Rust kernel code, rNVPCe DRAM backend fault injection, Asterinas ktests, fio/QEMU performance scripts.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/019-range-write-transactions/README.md`

## Global Constraints

- `write()` success remains recoverable after power loss.
- Data reaches persistence before the redirect commit boundary advances.
- One redirect entry contains at most 128 pages and requires logical, NV-slot, and SSD-block continuity.
- Migration never claims an entry while it is being extended.
- Failure to extend falls back to the existing independent-entry path unless persistence became uncertain.
- Existing uncommitted allocation-goal and rTIO worktree changes must remain intact.

## Review Focus

- Crash after new data persistence but before header publication recovers the old range only.
- Crash after header persistence recovers the extended range and every newly committed page.
- Migration, reclaim, or a non-adjacent slot prevents extension without corrupting the old entry.
- Pure overwrite does not publish an unchanged inode, while EOF growth still does.
- Worker queued-page and range-owner accounting follows the extended page count exactly once.

---

### Task 1: Pure-overwrite inode publication elision

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: `try_commit_existing_baseline_redirect_write(...)` and the current authoritative inode record.
- Produces: a pure-overwrite commit that publishes redirects at the current inode generation and returns the existing inode reference.

- [x] Add a failing integrity test asserting that a full-page overwrite leaves the authoritative inode generation and reference unchanged and remains readable after recovery.
- [x] Run the focused rNVPCe integrity ktest and verify the generation assertion fails.
- [x] Detect unchanged root, size, and block count in the existing-baseline fast path; use the current generation for redirect visibility and skip inode publication.
- [ ] Run the focused test and the complete rNVPCe integrity suite. (Focused test passes; the pre-existing unfiltered suite stalls in an EXT4 composite route test.)

### Task 2: Crash-safe persistent redirect extension

**Files:**
- Modify: `kernel/src/fs/rnvpc/descriptor.rs`
- Modify: `kernel/src/fs/rnvpc/persist.rs`
- Modify: `kernel/src/fs/rnvpc/layout.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produces: `NvRedirectExtentRecord::with_page_count(u16)` and a persistence primitive that persists data slots before atomically replacing the aligned header word of the selected redirect copy.

- [x] Add failing tests for old-range recovery before header persistence and extended-range recovery after header persistence.
- [x] Run them and verify failure occurs because redirect extension is unavailable.
- [x] Implement the 8-byte header update, selector stability checks, fault injection, and image-version bump.
- [x] Run the fault matrix and recovery tests.

### Task 3: DRAM open-entry extension

**Files:**
- Modify: `kernel/src/fs/rnvpc/redirect_index.rs`
- Modify: `kernel/src/fs/rnvpc/redirect_index/ownership.rs`
- Modify: `kernel/src/fs/rnvpc/frame/object_table.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Produces: an entry extension guard (`Dirty -> Extending -> Dirty`), atomic committed page count, exact range-owner growth, and worker queued-page growth.

- [x] Add failing tests that extend an entry, reject migration during extension, and verify owner/worker accounting.
- [x] Run them and verify the missing extension behavior fails.
- [x] Implement the extension state and accounting without adding a lock to the read lookup path.
- [x] Run redirect-index, ownership, and focused integrity tests.

### Task 4: Cross-system-call sequential append path

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/rnvpc/frame.rs`
- Modify: `kernel/src/fs/rnvpc/slot_allocator.rs`
- Modify: `kernel/src/fs/rnvpc/frame/redirect.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: the immediately preceding dirty redirect entry and an exact adjacent free NV slot range.
- Produces: a fast path that extends compatible entries and otherwise uses normal redirect publication.

- [x] Add a failing test issuing adjacent 4 KiB overwrite transactions and asserting one persistent/DRAM redirect entry with increasing page count.
- [ ] Add fallback tests for non-contiguous SSD targets, occupied adjacent slots, 128-page entries, and migrating entries.
- [x] Implement predecessor lookup, exact adjacent allocation, extension commit, and fallback.
- [ ] Run all rNVPCe tests and build the kernel. (Release build and focused tests pass; full suite limitation recorded above.)

### Task 5: Performance validation and documentation

**Files:**
- Modify: `docs/os_com_implementation/os_com_refactor_plan/019-range-write-transactions/README.md`
- Create: `docs/experiments/reports/RNVPCE-OPEN-REDIRECT-EXTENTS-2026-11-09.md`

**Interfaces:**
- Consumes: existing five-round overwrite sweep configuration.
- Produces: correctness evidence and before/after sequential/random overwrite results.

- [ ] Run focused 4 KiB sequential overwrite at 1, 4, 8, 12, and 16 jobs for five rounds.
- [ ] Run matching 4 KiB random overwrite points to detect regression.
- [ ] Compare redirect publication count, inode publication count, bandwidth, mean latency, and p99 latency against the 2026-11-05 baseline.
- [x] Record raw data location, exact commit/tree state, commands, and conclusions for the five-round 1-job validation.
