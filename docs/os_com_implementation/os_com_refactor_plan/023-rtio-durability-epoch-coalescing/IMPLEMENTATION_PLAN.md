# rTIO Durability Epoch Coalescing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Coalesce rTIO ready batches without a timer while preserving immediate tail and sync progress.

**Architecture:** Track submitted-but-incomplete and ready page counts in `DurabilityArbiter`. Busy workers close an epoch at the 1024-page writeback boundary; a sub-threshold tail closes only after candidate selection submits zero pages and no data BIO remains pending. Explicit drain always forces progress.

**Tech Stack:** Rust, Asterinas kernel threads, atomic counters, OSDK ktests, fio/QEMU real-device harness.

**Spec:** `docs/os_com_implementation/os_com_refactor_plan/023-rtio-durability-epoch-coalescing/README.md`

## Global Constraints

- No fixed coalescing delay or polling loop.
- No inode, entry-map or worker queue lock may be held over device FLUSH.
- NVM remains authoritative until the shared device FLUSH succeeds.
- rNVPC WAL behavior remains unchanged.

## Review Focus

- A callback racing initial submission accounting must not underflow pending pages.
- A tail smaller than 1024 pages must progress after candidate selection is idle and the last pending BIO completes.
- A post-snapshot completion must remain queued for a successor epoch.
- Explicit sync must not wait for the background threshold.
- Data/FLUSH failure must leave all page and owner counters balanced.

---

### Task 1: Account pending and ready pages

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/worker.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`

**Interfaces:**
- Produces: `note_submitted_pages`, page-aware `note_ready_batch`, and pending/ready snapshots.

- [x] Write ktests that assert submitted pages move exactly once from pending to ready.
- [x] Run the focused tests and observe failure because page accounting is absent.
- [x] Add atomic page accounting before BIO submission and at owner queue publication.
- [x] Run focused and aggregate tests.
- [x] Commit with message `Track rTIO durability page states`.

### Task 2: Gate background epoch closure

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration/durability.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: Task 1 pending/ready counters.
- Produces: coalesced background advance and forced drain advance.

- [x] Add failing tests for below-threshold defer, 1024-page threshold, immediate tail and force drain.
- [x] Run focused tests and verify the expected assertion failures.
- [x] Implement background eligibility and retain force mode for sync/drain.
- [x] Run focused tests, `ext4_research_routes`, package checks and formatting.
- [x] Commit with message `Coalesce rTIO durability epochs`.

### Task 3: Profile and validate the real-device matrix

**Files:**
- Modify: `docs/os_com_implementation/os_com_refactor_plan/023-rtio-durability-epoch-coalescing/README.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/README.md`
- Create: `docs/experiments/reports/RTIO-DURABILITY-EPOCH-COALESCING-2026-11-03.md`

**Interfaces:**
- Consumes: Task 2 implementation and existing 022 experiment harness.
- Produces: retained correctness and performance evidence.

- [x] Build and fingerprint profiling images with the source flag restored afterward.
- [x] Run the authorized serialized eight-point NVMe matrix with fio/sync/umount/e2fsck gates.
- [x] Compare epoch count, throughput, latency and capacity wait against 022.
- [x] Document exact pass/fail status and residual bottlenecks.
- [x] Commit with message `Validate rTIO durability epoch coalescing`.
