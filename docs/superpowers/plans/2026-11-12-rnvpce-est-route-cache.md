# rNVPCe EST Route Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reuse EXT4's existing extent status tree to avoid repeated NVM extent-tree traversal when rNVPCe plans overwrite writes over fully mapped SSD ranges.

**Architecture:** Add a generation-scoped rNVPCe SSD-route layer inside `EsCache`, separate from its native EXT4 mapping facts. The inode write path asks this layer for complete SSD coverage; redirect-complete writes bypass SSD routing, cache misses use the existing NVM walker and seed full validated extents.

**Tech Stack:** Rust kernel code, Asterinas ktests, QEMU fio on `/dev/nvme1n1p2`.

**Spec:** `docs/superpowers/specs/2026-11-12-rnvpce-est-route-cache-design.md`

## Global Constraints

- NVM inode/extent records remain authoritative for rNVPCe.
- Native EXT4 EST facts and rNVPCe route facts remain separate namespaces.
- Holes are never cached as rNVPCe route facts.
- A generation mismatch must miss and discard all older route facts.
- No I/O, blocking, or allocation while holding the EST spinlock.
- No new inode-global or filesystem-global lock.

## Review Focus

- A stale route from generation N must never be returned for generation N+1.
- A partial cached range must miss rather than treating the gap as a hole or mapping.
- Physical continuity and written state must survive cache record/merge/query exactly.
- Redirect-complete planning must not read the NVM extent tree.
- Allocation writes containing a hole must retain the original tree walk and predecessor goal.

---

### Task 1: Generation-Scoped EST Route Facts

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/inode/extent_manager/es.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`
- Test: `kernel/src/fs/fs_impls/ext4/inode/extent_manager/es.rs`

**Interfaces:**
- Produces: `ExtentManager::rnvpce_ssd_routes(generation, range)` and `ExtentManager::record_rnvpce_ssd_routes(generation, routes)` using a small internal route value type.

- [x] Add failing EST tests for full mapped hit, partial-range miss, generation rollover, overlapping replacement, physical discontinuity, and capacity eviction.
- [x] Run `ext4_extent_status_index` and confirm failure because the route API is absent.
- [x] Add the separate generation-scoped route map under the existing `EsCache` lock; query returns owned route values and never performs I/O while locked.
- [x] Expose narrow `ExtentManager` wrapper methods and rerun `ext4_extent_status_index` to green.

### Task 2: write plan Cache and Redirect Bypass

**Files:**
- Modify: `kernel/src/fs/rnvpc/write_transaction.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Test: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`

**Interfaces:**
- Consumes: Task 1 route lookup/record methods.
- Produces: a planner entry point accepting optional validated SSD routes and returning full tree-walk route facts for EST population.

- [x] Add failing ktests proving a cache-supplied plan performs zero NVM tree lookups, a redirect-complete plan also performs zero lookups, and a cache miss with a hole retains `NewAllocation` plus its predecessor goal.
- [x] Run focused rNVPCe plan tests and confirm the new fast-path assertions fail.
- [x] Refactor the tree walker to retain full validated extents while preserving clipped planning semantics and predecessor discovery.
- [x] Connect `Inode::write_rnvpce_once` to EST lookup/population under its existing inode write lock; keep generic planner callers on the uncached path.
- [x] Run focused planner, EST, integrity and ktest compile checks.

### Task 3: Performance and Report

**Files:**
- Modify: `docs/experiments/reports/RNVPCE-AUTHORITY-HOTPATH-2026-11-11.md`

- [x] Run 4/16/32-thread 4 KiB overwrite, three repetitions per point, on the existing fixed matrix.
- [x] Compare write-plan tree-read counts and bandwidth against the retained pre-EST results.
- [x] Run formatting, diff checks, focused ktests and post-run e2fsck validation.
- [x] Record implementation, raw paths, variance and any remaining bottleneck; retain the optimization because correctness holds and 4/16-thread throughput improves, while documenting the 32-thread regression explicitly.
