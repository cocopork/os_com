# Full E2E Device-Aligned Virtqueue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-run the full fio matrix for Asterinas EXT4, current rNVPCe-SU, and Linux NVLog with eight NVMe virtqueues and one SATA virtqueue, then archive comparable raw results, aggregates, and plots in a new directory.

**Architecture:** Keep the historical 2026-11-17 runner's default four-queue behavior reproducible, but add an explicit device-aligned queue profile shared by the new run. Asterinas uses one pinned IOThread per virtqueue; NVLog receives the same virtqueue count. A new orchestration script runs and validates the 648 Asterinas points plus 324 NVLog points before producing a separate 972-run archive.

**Tech Stack:** Bash, Python 3 `unittest`, QEMU/KVM, Docker, fio, matplotlib, cargo-osdk.

**Spec:** `docs/experiments/fio/formal/full-e2e-20261123/protocol.md`, with the user-approved changes recorded in this plan: omit EXT2; compare EXT4/rNVPCe-SU/NVLog; NVMe=8 queues and SATA=1 queue; write to a new archive.

## Global Constraints

- Never overwrite `docs/experiments/fio/formal/full-e2e-20261123/`.
- New archive: `docs/experiments/fio/formal/full-e2e-20261123/`.
- Real targets remain exactly `/dev/nvme1n1p2` (274877906944 bytes) and `/dev/sda4` (274877890560 bytes).
- NVMe uses 8 virtqueues and 8 IOThreads pinned to host CPUs 56--63; SATA uses 1 virtqueue and 1 IOThread pinned to CPU 63.
- Workloads, sweep coordinates, 2 GiB point size, `psync`, buffered I/O, fixed seed, and three repetitions remain identical to the 2026-11-17 matrix.
- The final matrix contains 972 runs and 324 aggregate points: 648 Asterinas EXT4/rNVPCe-SU runs plus 324 NVLog runs.
- Build Asterinas from the current checked-out source and record commit, dirty diff hash, ISO hash, and auxiliary-image hashes.
- Preserve final per-run `result.json`; remove large transient QEMU, mkfs, e2fsck, and NVDIMM logs only after all parsers and lifecycle checks pass.

## Review Focus

- A queue-count mismatch between the matrix and QEMU command must fail before the guest starts.
- IOThread CPU affinity must exclude fio CPUs and use exactly CPUs 56--63 for NVMe or CPU 63 for SATA.
- Existing 4/4 historical reproduction must remain the default when the new profile is not selected.
- Resume logic must reparse an existing point before skipping it and refuse incomplete point directories.
- Aggregation must reject missing, duplicate, or extra cases before writing the final archive.

---

### Task 1: Device-Aligned Queue Profile

**Files:**
- Modify: `docs/experiments/fio/formal/full-e2e-20261123/scripts/run-sweep.sh`
- Modify: `docs/experiments/fio/formal/full-e2e-20261123/scripts/test_sweep.py`
- Modify: `docs/experiments/fio/formal/full-e2e-20261123/scripts/nvlog/run-fio.sh`
- Test: `docs/experiments/fio/formal/full-e2e-20261123/scripts/nvlog/test_nvlog_fio_matrix.py`

**Interfaces:**
- Consumes: matrix device names `nvme` and `sata`.
- Produces: `virtqueues_for_device(device, profile)` semantics with historical `four` and new `device-aligned` profiles; QEMU receives the resolved count.

- [x] **Step 1: Add failing tests for historical 4/4 defaults and device-aligned NVMe=8/SATA=1 behavior**
- [x] **Step 2: Run the two runner test suites and verify the new assertions fail**
- [x] **Step 3: Add explicit queue-profile selection, dynamic IOThread construction, QMP queue mapping, and affinity validation**
- [x] **Step 4: Run both test suites and verify all assertions pass**

### Task 2: New Three-System Experiment Orchestration

**Files:**
- Create: `docs/experiments/<test-type>/<formal-or-temporary>/<experiment>/scripts/full-e2e-vq-aligned-20261120/run-all.sh`
- Create: `docs/experiments/<test-type>/<formal-or-temporary>/<experiment>/scripts/full-e2e-vq-aligned-20261120/test_run_all.py`
- Create: `docs/experiments/<test-type>/<formal-or-temporary>/<experiment>/scripts/full-e2e-vq-aligned-20261120/README.md`
- Create: `docs/experiments/fio/formal/full-e2e-20261123/README.md`
- Reuse: `docs/experiments/fio/formal/full-e2e-20261123/scripts/integrate_nvlog.py`

**Interfaces:**
- Consumes: frozen Asterinas source artifacts, frozen NVLog build artifacts, exact real-device paths, and the device-aligned runner profile.
- Produces: separate external Asterinas and NVLog raw roots plus a validated 972-row archive.

- [x] **Step 1: Add failing orchestration tests for exact modes, queue profile, output separation, and 972-run acceptance**
- [x] **Step 2: Run the orchestration tests and verify they fail before the script exists**
- [x] **Step 3: Implement resumable Asterinas then NVLog execution, integration, plotting, and cleanup gates**
- [x] **Step 4: Document destructive-device requirements, build provenance, commands, matrix, and acceptance criteria**
- [x] **Step 5: Run all full-e2e and NVLog script tests**

### Task 3: Build, Preflight, and Full Matrix Execution

**Files:**
- Create: `docs/experiments/fio/formal/full-e2e-20261123/results.csv`
- Create: `docs/experiments/fio/formal/full-e2e-20261123/aggregate.csv`
- Create: `docs/experiments/fio/formal/full-e2e-20261123/plots/*.png`
- Create: `docs/experiments/fio/formal/full-e2e-20261123/raw/*/result.json`

**Interfaces:**
- Consumes: Task 1 queue support and Task 2 orchestrator.
- Produces: the final three-system comparison archive.

- [x] **Step 1: Build the current Asterinas release ISO with diagnostics disabled and record source/image provenance**
- [x] **Step 2: Run one point per system and device, verify queue count/affinity, byte count, sync/unmount, and read-only e2fsck**
- [x] **Step 3: Run the complete resumable 972-point matrix**
- [x] **Step 4: Require exactly 972 unique results, three samples per 324 coordinate groups, and 20 plots (18 per-workload plots plus two bandwidth summaries)**
- [x] **Step 5: Visually inspect representative bandwidth and latency plots and run `git diff --check` plus all script tests**
- [x] **Step 6: Remove only transient logs named by the cleanup manifest after final validation; retain all `result.json` and CSV inputs**
