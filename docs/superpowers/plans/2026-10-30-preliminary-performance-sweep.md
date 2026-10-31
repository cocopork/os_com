# Preliminary Performance Sweep Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and execute the approved 108-point NVMe sweep comparing native EXT4, rNVPC WAL+rTIO, and rNVPCe+rTIO.

**Architecture:** A host runner validates and reformats only the authorized partition, injects a point configuration into a private boot image, and launches one isolated VM per point. A guest script validates the selected mount mode, runs initialization and fio, syncs and unmounts. A parser rejects incomplete points and emits per-point and curve summaries.

**Tech Stack:** Bash, Python 3 standard library, fio 3.39, Docker, QEMU/KVM, debugfs, e2fsck.

**Spec:** `docs/os_com_implementation/test/preliminary-performance-sweep/README.md`

## Global Constraints

- Use only `/dev/nvme1n1p1`, after the existing explicit erase authorization and after verifying it is an unmounted 256 GiB partition.
- Use the current release image, 64 vCPUs, 8 GiB RAM, a 128 MiB simulated NVDIMM pool, and the existing CPU-isolation layout.
- Run 108 serialized points: three modes, two workloads, two axes, nine coordinates.
- Preserve raw logs outside Git and track only scripts and the reviewed report.
- Reject any point without exact fio bytes, successful sync/unmount, completion marker, expected runtime journal mode, and clean post-run e2fsck.

## Review Focus

- A mode token that silently selects native EXT4 must fail before fio.
- A runtime JBD2 state that disagrees with the mode contract must fail before fio.
- Random initialization must cover every job's complete measured file range.
- Non-divisible 2 GiB thread points must validate the floor-divided actual byte count.
- Resume must skip only fully validated points and must never overwrite incomplete evidence.

---

### Task 1: Matrix and Result Parser

**Files:**
- Create: `docs/experiments/scripts/preliminary-performance-sweep-20261030/sweep.py`
- Test: `docs/experiments/scripts/preliminary-performance-sweep-20261030/test_sweep.py`

**Interfaces:**
- Produces: `matrix() -> list[Point]`, `parse_log(path: Path, point: Point) -> dict`, and CSV/report-table output.

- [x] Write tests for 108 unique case names, exact axes, floor-divided bytes, valid parsing, and rejection of each missing completion condition.
- [x] Run the tests and confirm they fail before the implementation exists.
- [x] Implement the matrix, parser, resume validation, CSV writer, and Markdown table writer.
- [x] Run the tests and confirm all pass.

### Task 2: Guest and Host Runners

**Files:**
- Create: `docs/experiments/scripts/preliminary-performance-sweep-20261030/guest.sh`
- Create: `docs/experiments/scripts/preliminary-performance-sweep-20261030/run-sweep.sh`

**Interfaces:**
- Consumes: point configuration fields emitted by `sweep.py`.
- Produces: one serial log and one e2fsck log per point, plus explicit configuration, journal-state, fio, sync, and completion markers.

- [x] Add shell syntax and static marker tests for all three mount configurations and journal expectations.
- [x] Implement guest initialization, mount validation, fio, sync, and unmount behavior.
- [x] Implement host device validation, per-mode formatting, VM launch, completion checks, e2fsck, and validated resume.
- [x] Run shell syntax, Python tests, and `git diff --check`.

### Task 3: Smoke Validation and Full Sweep

**Files:**
- Create: external artifact directory `/home/zn/asterinas-build-env/preliminary-performance-sweep-20261030-current/`

**Interfaces:**
- Consumes: current release ISO and Tasks 1-2 runners.
- Produces: validated raw logs and `results.csv`.

- [x] Run one 4 KiB sequential 64 MiB smoke point for each mode and verify the mode/journal markers, exact bytes, unmount, and e2fsck.
- [ ] Run all 108 2 GiB points serially, resuming only points accepted by the parser.
- [ ] Parse all points and verify 108 accepted results with no missing coordinates.

### Task 4: Reviewed Experiment Report

**Files:**
- Create: `docs/experiments/reports/PRELIMINARY-PERFORMANCE-SWEEP-2026-10-30.md`

**Interfaces:**
- Consumes: validated `results.csv` and raw evidence manifests.
- Produces: complete thread/bs tables, ratios, observed peaks, failures, and evidence boundaries.

- [x] Generate the comparison tables for the 70 strictly valid random/sequential thread and block-size points; label missing/invalid coordinates.
- [x] Analyze scaling, fio-to-sync gaps, repeated 4 KiB/1-job variation, and anomalous coordinates without assigning unsupported causes.
- [x] Verify report values against CSV and record commit, ISO hash, configuration, point counts, and residual limitations.
