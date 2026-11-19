# Mixed Sync Ratio Benchmark Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and validate a deterministic non-fio benchmark and experiment harness for the two-device EXT2/EXT4/rNVPCe mixed `write()+fsync()` matrix.

**Architecture:** A static pthread C program generates counter-based deterministic 4 KiB operations and emits one JSON result. Python defines and validates the matrix, while a shell runner reuses the repository's destructive-device preflight, QEMU, virtqueue affinity, filesystem preparation, cleanup, and e2fsck pattern. Aggregation and plotting consume only validated per-run JSON.

**Tech Stack:** C11/pthreads/POSIX I/O, Python 3 standard library and matplotlib, POSIX shell, Docker, QEMU/KVM, Asterinas, e2fsprogs.

**Spec:** [`docs/superpowers/specs/2026-11-19-mixed-sync-ratio-benchmark-design.md`](../specs/2026-11-19-mixed-sync-ratio-benchmark-design.md)

## Global Constraints

- Use 8 threads, one preallocated file and ordinary file descriptor per thread.
- Use 4 KiB page-aligned random operations and fixed counter-derived decisions.
- Test read/write ratios 0:10, 3:7, 5:5, and 7:3 and `fsync` injection ratios 0, 20, 40, 60, 80, and 100 percent.
- Warm up for 10 seconds and measure for 30 seconds.
- Prewrite and sync eight 320 MiB files before timing; measured writes must not allocate file data or extend files.
- Record a 256 MiB configured data-cache budget: EXT4 256 MiB DRAM; rNVPCe 128 MiB DRAM plus 128 MiB NV data.
- Mark current EXT2 cache capacity as uncontrolled instead of claiming strict 256 MiB enforcement.
- Use one SATA virtqueue and eight NVMe virtqueues; save queue/thread affinity evidence.
- Run every point three times and retain raw results, aggregate CSV, plots, and a Chinese reproduction report.
- Do not modify or discard unrelated dirty worktree changes.

## Review Focus

- A branch in read/write or sync selection must not perturb later page addresses; the deterministic-sequence tests pin all three decision streams.
- A selected sync operation must call `fsync()` only after its `pwrite()` succeeds; failure injection tests require a nonzero process result and no false operation count.
- Concurrent start/stop must not count warm-up operations or truncate a thread's latency sample; lifecycle tests validate phase boundaries.
- Existing files must have exactly the configured size before measurement; guest integration tests reject sparse, short, or newly extended files.
- Resume logic must never accept a partial point; parser and runner tests require valid result JSON, cleanup markers, and e2fsck status.

---

### Task 1: Deterministic mixed-I/O benchmark

**Files:**
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/mixed_sync_bench.c`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/Makefile`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/test_mixed_sync_bench.py`

**Interfaces:**
- Consume CLI options `--directory`, `--threads`, `--file-bytes`, `--read-percent`, `--sync-percent`, `--seed`, `--warmup-seconds`, and `--runtime-seconds`.
- Produce a single JSON object on stdout and a nonzero exit status on any thread, I/O, timing, or configuration error.

- [x] Write host tests that compile the program and assert deterministic operation classes, nested sync selections, one file per thread, valid ratio rejection, and JSON schema.
- [x] Run `python3 docs/experiments/scripts/mixed-sync-ratio-20261119/test_mixed_sync_bench.py` and verify the tests fail because the program is absent.
- [x] Implement counter-based decisions, pthread barriers, separate warm-up/measurement counters, `pread`/`pwrite`/post-write `fsync`, and bounded per-thread latency histograms.
- [x] Add a deterministic `--self-test` mode so algorithm invariants can run without a mounted test filesystem.
- [x] Run the focused test and verify all cases pass under the host and tools container compilers.

### Task 2: Matrix and guest execution

**Files:**
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/matrix.py`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/guest.sh`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/test_matrix.py`

**Interfaces:**
- Consume the benchmark binary plus a generated `case.cfg`.
- Produce exact case names/configuration rows and guest completion markers enclosing the benchmark JSON.

- [x] Write failing matrix tests for 432 points, exact case uniqueness, three repeats, all ratio coordinates, 2560 MiB total prewrite, and device-specific queue counts.
- [x] Implement typed matrix generation and strict case lookup without accepting unknown coordinates.
- [x] Write guest-script contract tests that require native prewrite plus sync before the measured mount, eight existing 320 MiB files, mode-specific mount options, and post-run sync/unmount markers.
- [x] Implement EXT2, EXT4, and rNVPCe mounts; set EXT4 cache to 256 MiB and rNVPCe DRAM/NV data budgets to 128 MiB each; emit the EXT2 uncontrolled-capacity annotation.
- [x] Run the matrix and guest contract tests and verify they pass.

### Task 3: Safe two-device runner and result validation

**Files:**
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/run-sweep.sh`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/parse_result.py`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/test_parse_result.py`

**Interfaces:**
- Consume source ISO/initramfs images, explicit result root, optional exact `CASES`, and the Task 2 matrix.
- Produce one immutable point directory with configuration, hashes, QEMU/affinity logs, raw JSON, parsed `result.json`, cleanup evidence, and e2fsck output.

- [ ] Write failing parser tests for valid output and rejection of wrong seed, ratios, operation counts, prewrite bytes, missing cleanup, failed I/O, and failed e2fsck.
- [ ] Implement parsing and validate reported actual ratios against operation counts rather than expected percentages alone.
- [ ] Adapt the existing full-e2e destructive target preflight and exact device-size checks without broadening device paths.
- [ ] Build/inject the benchmark, configure 1 SATA or 8 NVMe virtqueues/IOThreads, pin IOThreads away from benchmark CPUs, and save QMP queue evidence.
- [ ] Implement resume only for fully reparsed points; preserve incomplete/failed evidence and never overwrite it automatically.
- [ ] Run shell syntax checks, parser tests, and one non-destructive mocked runner test.

### Task 4: Aggregation, plotting, and reproduction report

**Files:**
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/summarize.py`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/plot_results.py`
- Create: `docs/experiments/scripts/mixed-sync-ratio-20261119/test_reporting.py`
- Create: `docs/experiments/test_results/mixed-sync-ratio-20261119/README.md`

**Interfaces:**
- Consume 432 validated result objects.
- Produce `results.csv`, `aggregate.csv`, plotting-source CSV, per-device/per-read-write-ratio throughput and latency plots, and a Chinese reproduction guide.

- [ ] Write failing tests for three-sample grouping, arithmetic mean/stddev/min/max, categorical 0--100 percent x-axis labels, raw-sample markers, and missing-point rejection.
- [ ] Implement aggregation without treating failed or absent rounds as zero.
- [ ] Implement plots whose x coordinates correspond one-to-one with the six sync percentages and whose legends distinguish filesystems and individual rounds.
- [ ] Document device destruction requirements, build hashes, 256 MiB cache-budget accounting, the EXT2 limitation, commands, expected artifact counts, and validation criteria.
- [ ] Run all reporting tests and generate plots from a synthetic complete matrix.

### Task 5: Build and experimental smoke validation

**Files:**
- Modify: `docs/experiments/test_results/mixed-sync-ratio-20261119/README.md`

**Interfaces:**
- Consume the completed Tasks 1--4 and the current release kernel image.
- Produce evidence that the benchmark is executable in Asterinas and two representative real-device points are structurally valid before launching the full destructive matrix.

- [ ] Run formatting, all new unit/contract tests, C warnings-as-errors build, shell syntax checks, and Python compilation.
- [ ] Build a release image with 128 MiB rNVPCe data capacity and the documented worker/CPU isolation settings.
- [ ] With existing explicit device authorization revalidated, run one EXT4 and one rNVPCe smoke point and verify prewrite, benchmark JSON, sync, unmount, and e2fsck.
- [ ] Record actual hashes, cache configuration/observations, queue configuration, and smoke results in the reproduction report.
- [ ] Stop before the 432-point full matrix unless the user explicitly requests launching the complete destructive run.
