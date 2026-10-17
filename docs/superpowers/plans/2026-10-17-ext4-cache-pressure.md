# EXT4 Cache Pressure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Sustain native buffered EXT4 I/O beyond guest RAM with dirty backpressure and safe clean reclaim.
**Architecture:** Optional shared PageCache accounting follows frame lifetime and dirty/writeback transitions. A weak filesystem worker writes bounded batches and reclaims uniquely referenced clean pages; chunked buffered I/O reserves headroom before inode/journal locks.
**Tech Stack:** Safe Rust, OSTD threads/WaitQueue/Frame, existing EXT4 BIO/JBD2, Docker/QEMU kernel tests and fio.
**Spec:** docs/superpowers/specs/2026-10-17-ext4-cache-pressure-design.md

## Global Constraints

- Default resident high watermark 512 MiB; dirty high 128 MiB; background dirty trigger 64 MiB.
- Buffered chunks at most 256 KiB, plus unaligned tail capacity.
- No blocking page locks or backend I/O under page-array spinlock in reclaim; use the existing XArray RCU retirement mechanism, with no mapped/pinned-page eviction. XArray retirement itself may allocate callback storage.
- Preserve NV protocols and journal ordering. Use previously authorized test partition only.

## Review Focus

- Redirty during BIO and BIO failure must retain pressure and newest data.
- Mapping/read/BIO references must exclude reclaim; cold rereads must recover identical bytes.
- Large and concurrent calls must not bypass admission; waiting must release inode/journal locks.
- Partial submission must drain already-submitted BIOs on error.
- Worker ownership and inode registry must not prevent unmount or starve other inodes.

### Task 1: PageCache accounting and safe bounded operations

Files: `kernel/src/vm/page_cache/{accounting.rs,cache_page.rs,mod.rs,vmo/mod.rs,vmo/options.rs,tests/mod.rs}`.
Interfaces: `PageCacheAccounting::{resident_pages,dirty_pages,writeback_pages}`; `VmoOptions::accounting`; `PageCache::{flush_some_pages,reclaim_clean_pages}`.
- [x] Add failing tests for frame-lifetime accounting, repeated dirty/redirty/failure, pinned-page reclaim and bounded flush.
- [x] Run focused OSDK tests and record RED.
- [x] Implement optional accounting and bounded flush/reclaim; always drain partial BIO submissions.
- [x] Run PageCache regressions and record GREEN.

### Task 2: Native EXT4 pressure controller

Files: `kernel/src/fs/fs_impls/ext4/{writeback.rs,mod.rs,fs.rs,inode/mod.rs}`; tests in writeback.rs.
Interfaces: shared per-FS `CachePressure`, RAII admission reservation; bounded inode writeback; weak native worker and inode registry.
- [x] Add pressure tests using small test thresholds, including multiple files, failure and rereads.
- [x] Run tests RED, implement admission/chunking/background worker, run GREEN.
- [x] Review locking, mapping frame ownership and worker teardown.

### Task 3: Integration and real-device validation

Files: recovery report and external benchmark artifacts.
- [x] Build kernel; run EXT4 and PageCache regressions, review diff, check formatting.
- [x] Rebuild release ISO; repeat 16 GiB 4 KiB random and 128 KiB sequential buffered workloads, 8 GiB guest; retain host device counters and fio output.
- [x] Record measured results and remaining limits. Leave changes reviewable without automatic push.

## Execution ledger

- Builder check: no important design questions; verify tracked PTE frame references in implementation.
- Ruling: continue directly under the user's implementation request and existing benchmark authorization; do not add another approval handoff.
- Preserve pre-existing untracked docs/recovery/FEATURE-COMPLETENESS-CHECKLIST.md.

- RED artifacts: cache-pressure-red.log (missing APIs); green-02/05 found RCU lifecycle and failed-completion issues; cache-pressure-debug.log caught short-read failure; cache-single-failure-red.log confirms single-page completion lost dirty state. All logs under /home/zn/asterinas-build-env/.
- GREEN: ext4-writeback-20261017/regressions.log reports 584 passed, 0 failed, 158 filtered out. 582 unique selected function names also select two matching tests elsewhere. SMP=1: OSDK does not boot APs; cooperative workers and explicit peer task switches exercise RCU grace periods without the normal timer/idle service.
- Ruling: account regular-file backed frames only; directory metadata retains existing journal ownership. Failed completion must re-dirty in IRQ context without taking page locks; use charge-before-CAS and reuse callback in single/batch paths.
- Independent review found readonly reclaim starvation, EOF writer advancement and single-page error fallback; all corrected with regressions. Reviewer found no remaining blocker for device testing after fixes.
- Device RED: `wb-seq128k-j1-16g.log` returned ENOMEM at exactly 512 MiB. Idle CPUs did not report RCU grace periods, keeping retired frames charged. OSTD idle now reports quiescence outside bootstrap, with atomic-mode validation repeated after observing the one-way bootstrap transition. Reviewer confirmed the safety boundary.
- Final release build: `build-release-final-guard.log`, exit 0; frozen `ext4-cache-pressure-final-guard.iso`. Final regression rerun: `regressions.log`, 584 passed / 0 failed. `rustfmt-check.log` and `git diff --check` pass. Artifacts are in `/home/zn/asterinas-build-env/ext4-writeback-20261017/`.
- Device GREEN before the final bootstrap-only safety refinement: SMP16 seq128k completed 16 GiB; `final-seq128k-j1-16g-e2fsck.log` returns 0. Final-ISO device tests continue; fresh-file random writes are time-bounded, so report actual bytes rather than claiming the requested 16 GiB range was fully written. Add prefilled random overwrites to separate extent-allocation cost.
- Additional SMP16 RED: concurrent sequential writers stalled in the live-queue `WaitQueue::wake_all` drain; a single call reached 27,849,470 successful wakes while woken tasks immediately requeued. Snapshot broadcast fixes termination without holding the queue lock while notifying the scheduler. Deterministic real-Waker test: old loop returned 9 instead of 1 (`wait-broadcast-red-ostd.log`); snapshot passes, including retained requeue notification. Independent review confirmed semantics.
- Final snapshot-source regressions: kernel `regressions.log` has 585 passed / 0 failed, including 8 concurrent native writers with content rereads. OSTD `wait-broadcast-green-ostd.log` has 7 passed / 0 failed. OSDK selects the current package: run OSTD tests from `/root/asterinas/ostd` with function-name selections; a kernel-package run or module-path filter would select zero OSTD tests.

- Fragmented-page RED: SMP16 prefilled random writes stalled in allocator `balance_to` while holding the global lock; fallback excluded order=0. Include single-page transfer and prune every empty recursive branch. `allocator-balance-red-cleanup.log` fails the exact transfer assertion with mock pools drained; `allocator-balance-green.log` passes all 11 allocator tests. Independent review confirmed transfer bounds and cleanup. Final kernel/real-device reruns use this allocator fix.

- Further device RED: random write reported ENOMEM/pagetable panic despite 7,619,336 KiB MemFree immediately before error. Fixed cache refill to retry the caller-sized segment when the larger optimization batch cannot be allocated. Real BuddySet tests force refill failure while 1/2/3/4-page requests remain satisfiable; `allocator-refill-red.log` fails None vs Some, `allocator-refill-green.log` passes all 13 allocator tests. Empty-failure case uses a callback. Independent review found no blocker; final regression/device reruns required.

- Decisive pool diagnosis: 32 KiB failure with global=4,216,102,912 bytes but global_orders=0x7 (only 4/8/16 KiB), and fragmented local pool. Batch fallback alone was insufficient. LOCAL_POOL now uses an IRQ SpinLock; after normal failure, consolidate buddy pools one local->global lock pair at a time and retry once, plus retry after balancing. No heap allocation, global->local lock, or two-local lock hold in the sweep. Cross-three-pool RED/GREEN proves coalescing and frame conservation; whole allocator has 14 passing tests. Independent review confirmed lock order and metadata ownership.

- FINAL: consolidated release build exits0; frozen ISO SHA256 08d2368c522ee702564c8577e728ffef6615d543dac773a19a332990ef8c0cd9. Kernel 585/0, allocator14/0, WaitQueue7/0, rustfmt and diff checks pass. All four SMP16/8GiB guest cases write full16GiB, fio error0, sync/unmount success, QEMU33 and readonly e2fsck0; suite exits0. seq128k j1/j16=285.33/284.17 MiB/s; prefilled rand4k j1/j16=239.54/156.67 MiB/s. Results and exact source snapshots retained in external artifact directory; no automatic push.
