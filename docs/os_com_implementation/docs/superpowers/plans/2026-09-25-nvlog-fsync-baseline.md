# NVLog Fsync Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and run a safe 36-point NVLog fsync matrix that matches `aster-ext4-baseline-test` and emits its established summary schema.

**Architecture:** A Python host controller expands the matrix, validates the two approved physical partitions, stages the existing NVLog kernel/initramfs, runs one QEMU guest per point, and parses self-describing serial markers. A container launcher verifies block-device identity and builds a run-local initramfs; a guest script initializes a 256 MiB NVLog pool and executes one baseline-compatible fio point.

**Tech Stack:** Python 3 standard library, Bash/POSIX shell, QEMU/KVM, fio, EXT4, NVLog/NVPC, Docker.

**Spec:** `docs/superpowers/specs/2026-09-25-nvlog-fsync-baseline-design.md`

## Global Constraints

- Real runs require `--confirm-destroy` and accept only `/dev/sda4` and `/dev/nvme1n1p2`.
- Run exactly 36 fsync points: two devices, two workloads, and `njobs=1,4,8,12,16,20,24,28,32`.
- Use `runtime=30`, `ioengine=psync`, `iodepth=1`, `direct=0`, `fsync=1`, `fallocate=none`, and a 2 GiB aggregate file range.
- Use 64 vCPUs, 4 GiB RAM, a 512 MiB PMem backend, and a 256 MiB NVLog effective pool (`65536` pages).
- Each point has a 180-second wall-clock limit and runs serially in a fresh VM after reformatting the selected partition.
- Preserve the original NVLog and Asterinas result directories; write only a new timestamped result directory.

---

### Task 1: Matrix, parsing, and safety model

**Files:**
- Create: `test/aster-ext4-baseline-test/run_nvlog_fsync_baseline.py`
- Create: `test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py`

**Interfaces:**
- Produces: `Point(device: str, device_path: Path, workload: str, njobs: int)`.
- Produces: `matrix_points(devices, workloads, njobs) -> list[Point]`.
- Produces: `parse_serial(point: Point, serial: str, qemu_status: int) -> ResultRow`.
- Produces: `validate_device(path: Path) -> str`, returning `major:minor`.
- Produces: `detect_kernel_failures(serial: str) -> list[str]`.

- [ ] **Step 1: Write matrix and allowlist tests**

```python
def test_default_matrix_has_exactly_36_fsync_points(self):
    points = runner.matrix_points(runner.DEVICES, runner.WORKLOADS, runner.NJOBS)
    self.assertEqual(len(points), 36)
    self.assertEqual({p.workload for p in points}, {"randwrite-4k", "seqwrite-4k"})
    self.assertEqual({p.njobs for p in points}, {1, 4, 8, 12, 16, 20, 24, 28, 32})

def test_only_dedicated_partitions_are_authorized(self):
    self.assertEqual(
        runner.AUTHORIZED_DEVICES,
        {"sata": Path("/dev/sda4"), "nvme": Path("/dev/nvme1n1p2")},
    )
```

- [ ] **Step 2: Run the focused tests and verify they fail**

Run: `python3 -m unittest -v test_nvlog_fsync_baseline.NVLogFsyncBaselineTest.test_default_matrix_has_exactly_36_fsync_points test_nvlog_fsync_baseline.NVLogFsyncBaselineTest.test_only_dedicated_partitions_are_authorized`

Expected: import or attribute failures because the controller does not exist.

- [ ] **Step 3: Implement the immutable matrix model**

```python
AUTHORIZED_DEVICES = {"sata": Path("/dev/sda4"), "nvme": Path("/dev/nvme1n1p2")}
WORKLOADS = ("randwrite-4k", "seqwrite-4k")
NJOBS = (1, 4, 8, 12, 16, 20, 24, 28, 32)

@dataclass(frozen=True)
class Point:
    device: str
    device_path: Path
    workload: str
    njobs: int

def matrix_points(devices, workloads, njobs):
    return [Point(name, devices[name], workload, jobs)
            for name in devices for workload in workloads for jobs in njobs]
```

- [ ] **Step 4: Add parser tests with success, timeout, and kernel-failure fixtures**

```python
def test_parse_serial_emits_existing_summary_schema(self):
    row = runner.parse_serial(self.point, SUCCESS_SERIAL, 0)
    self.assertEqual(row.fs, "nvlog")
    self.assertEqual(row.mode, "fsync")
    self.assertEqual(row.valid, 1)
    self.assertGreater(row.drain_bw_mib_s, 0)

def test_soft_lockup_is_invalid(self):
    row = runner.parse_serial(self.point, SUCCESS_SERIAL + "\nwatchdog: BUG: soft lockup", 0)
    self.assertEqual(row.valid, 0)
    self.assertIn("soft lockup", row.note.lower())
```

- [ ] **Step 5: Implement marker parsing and validity checks**

Parse `NVLOG_FSYNC_FIO_JSON_BEGIN/END` and `NVLOG_FSYNC_DRAIN busy_ns=<n> drain_ns=<n>`. Reject nonzero fio errors, missing drain markers, less than 10 MiB written, nonzero QEMU status, and serial signatures matching `Kernel panic`, `Oops:`, `BUG:`, `soft lockup`, or RCU stall. Return fields in this exact order:

```python
SUMMARY_FIELDS = (
    "device", "fs", "workload", "mode", "njobs", "write_mib",
    "busy_ms", "drain_ms", "busy_bw_mib_s", "drain_bw_mib_s",
    "iops", "valid", "note",
)
```

- [ ] **Step 6: Run all pure Python tests**

Run: `python3 -m unittest -v test_nvlog_fsync_baseline.py`

Expected: all matrix, parser, and safety-model tests pass without Docker or root access.

- [ ] **Step 7: Commit Task 1**

```bash
git add test/aster-ext4-baseline-test/run_nvlog_fsync_baseline.py test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py
git commit -m "test: add NVLog fsync baseline model"
```

### Task 2: Guest fsync point executor

**Files:**
- Create: `test/aster-ext4-baseline-test/nvlog_fsync_guest.sh`
- Modify: `test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py`

**Interfaces:**
- Consumes kernel command line variables `NVLOG_DEV`, `NVLOG_WORKLOAD`, `NVLOG_NJOBS`, `NVLOG_RUNTIME`, and `NVLOG_FILE_RANGE_MB`.
- Produces serial markers `NVLOG_FSYNC_BEGIN`, `NVLOG_FSYNC_PARAM`, `NVLOG_FSYNC_FIO_JSON_BEGIN/END`, `NVLOG_FSYNC_DRAIN`, and `NVLOG_FSYNC_SUCCESS`.

- [ ] **Step 1: Add a static contract test for exact fio and NVLog settings**

```python
def test_guest_script_has_baseline_fsync_contract(self):
    script = (HERE / "nvlog_fsync_guest.sh").read_text()
    for token in ("nvpc_sz=65536", "--ioengine=psync", "--iodepth=1",
                  "--direct=0", "--fsync=1", "--fallocate=none",
                  "--filename_format=f.$jobnum"):
        self.assertIn(token, script)
```

- [ ] **Step 2: Run the contract test and verify it fails**

Run: `python3 -m unittest -v test_nvlog_fsync_baseline.NVLogFsyncBaselineTest.test_guest_script_has_baseline_fsync_contract`

Expected: failure because `nvlog_fsync_guest.sh` does not exist.

- [ ] **Step 3: Implement guest initialization and cleanup**

The script mounts proc/sys/devtmpfs, recreates a devdax namespace, runs:

```sh
nvpcctl config 'extend_lru=1 absorb_syn=1 promote_level=4 demote_before_promote=1 nvpc_lru_evict=1 nvpc_sz=65536 rebuild=1 force_start=1'
daxctl reconfigure-device --mode=nvpc all --no-online
/benchmark/fio/ext4_perf_randwrite/mkfs/mke2fs -t ext4 -F -q "$device"
mount -t ext4 "$device" /mnt/test
nvpcctl open /mnt/test
```

Its EXIT trap must stop background jobs, sync, close NVLog, unmount, emit success only on status zero, and power off.

- [ ] **Step 4: Implement the single fio point**

Compute `per_job_mb = floor((2048 / njobs) / 4) * 4`, map random to `randwrite` and sequential to `write`, and run:

```sh
fio --output-format=json --name="${workload}-fsync-nj${njobs}" \
  --ioengine=psync --numjobs="$njobs" --iodepth=1 --thread=1 \
  --group_reporting=1 --runtime="$runtime" --direct=0 --fsync=1 \
  --norandommap=0 --directory=/mnt/test --filename_format='f.$jobnum' \
  --nrfiles=1 --filesize="${per_job_mb}M" --rw="$rw" --bs=4k \
  --size="${per_job_mb}M" --create_on_open=1 --fallocate=none
```

Capture monotonic nanoseconds before fio, after fio, and after `sync -f /mnt/test`; emit JSON and drain markers even when fio returns nonzero so the host can diagnose the point.

- [ ] **Step 5: Validate shell syntax and contract tests**

Run: `sh -n nvlog_fsync_guest.sh && python3 -m unittest -v test_nvlog_fsync_baseline.py`

Expected: shell syntax and all tests pass.

- [ ] **Step 6: Commit Task 2**

```bash
git add test/aster-ext4-baseline-test/nvlog_fsync_guest.sh test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py
git commit -m "test: add NVLog fsync guest workload"
```

### Task 3: Container QEMU launcher and physical-device guard

**Files:**
- Create: `test/aster-ext4-baseline-test/nvlog_fsync_container.sh`
- Modify: `test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py`

**Interfaces:**
- Consumes positional arguments `RUN_ID HOST_DEVICE EXPECTED_MAJ_MIN WORKLOAD NJOBS`.
- Consumes staged files under `/root/asterinas/research-test-images/nvlog-smoke`.
- Produces the QEMU command block and returns QEMU's exit status.

- [ ] **Step 1: Add a dry-run command test**

```python
def test_container_script_pins_vm_and_storage_configuration(self):
    script = (HERE / "nvlog_fsync_container.sh").read_text()
    for token in ("-smp 64", "-m 4G", "size=536870912",
                  "queue-size=1024", "num-queues=1", "timeout 180"):
        self.assertIn(token, script)
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `python3 -m unittest -v test_nvlog_fsync_baseline.NVLogFsyncBaselineTest.test_container_script_pins_vm_and_storage_configuration`

Expected: failure because `nvlog_fsync_container.sh` does not exist.

- [ ] **Step 3: Implement identity and allowlist checks**

Accept only `/dev/sda4` or `/dev/nvme1n1p2`; require `EXPECTED_MAJ_MIN` to match `^[0-9]+:[0-9]+$`; resolve `/sys/dev/block/$EXPECTED_MAJ_MIN`; verify its kernel partition name matches the requested path; create a run-local block proxy with `mknod` only when the container lacks the named device node.

- [ ] **Step 4: Implement run-local initramfs and QEMU command**

Use `/tmp/nvlog-fsync-$RUN_ID` and `/dev/shm/nvlog-fsync-$RUN_ID.pmem`. Inject the guest script and module metadata into a copied initramfs. Launch a single virtio-blk queue with `cache=none,aio=threads,queue-size=1024,num-queues=1`, a 512 MiB `memory-backend-file`, a 16 MiB NVDIMM label, 64 vCPUs, and 4 GiB RAM.

- [ ] **Step 5: Implement scoped cleanup**

Store the QEMU PID inside the run-local directory. `--cleanup RUN_ID` may terminate only a process whose `/proc/<pid>/cmdline` contains the same validated run-local prefix, then remove only the two paths derived from that run ID.

- [ ] **Step 6: Run shell and unit tests**

Run: `bash -n nvlog_fsync_container.sh && python3 -m unittest -v test_nvlog_fsync_baseline.py`

Expected: all tests pass.

- [ ] **Step 7: Commit Task 3**

```bash
git add test/aster-ext4-baseline-test/nvlog_fsync_container.sh test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py
git commit -m "test: add guarded NVLog QEMU launcher"
```

### Task 4: Host orchestration, smoke, and full matrix

**Files:**
- Modify: `test/aster-ext4-baseline-test/run_nvlog_fsync_baseline.py`
- Modify: `test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py`
- Modify: `test/aster-ext4-baseline-test/README.md`

**Interfaces:**
- Consumes CLI flags `--devices`, `--workloads`, `--njobs`, `--runtime`, `--timeout`, `--confirm-destroy`, and `--dry-run`.
- Produces `parameters.json`, `summary.csv`, `result_table.txt`, per-point serial logs, fio JSON, and `runner.log`.

- [ ] **Step 1: Add dry-run, confirmation, and output tests**

```python
def test_real_run_requires_confirmation(self):
    with self.assertRaisesRegex(RuntimeError, "confirm-destroy"):
        runner.validate_request(runner.parse_args([]))

def test_dry_run_reports_36_points(self):
    payload = runner.plan_payload(runner.default_points())
    self.assertEqual(payload["point_count"], 36)
    self.assertEqual({point["mode"] for point in payload["points"]}, {"fsync"})
```

- [ ] **Step 2: Run the new tests and verify they fail**

Run: `python3 -m unittest -v test_nvlog_fsync_baseline.py`

Expected: failures for missing orchestration functions.

- [ ] **Step 3: Implement staging and preflight**

Import the existing NVLog staging helpers from `research1/evaluation/comparision-objs/NVLog/run_nvlog_smoke.py`; stage its kernel, base initramfs, and generated module metadata plus the two new scripts. Verify Docker container state, host tools, device block type, unmounted state, major/minor identity, and absence from live QEMU command lines before creating a result directory.

- [ ] **Step 4: Implement serial execution and durable result updates**

For each point, write the exact Docker command, stream stdout to its serial log and `runner.log`, enforce the host-side timeout, call scoped container cleanup on exit, parse a `ResultRow`, and rewrite `summary.csv` atomically after every completed point. Continue after invalid points.

- [ ] **Step 5: Implement result artifacts and README usage**

Document these commands:

```bash
python3 run_nvlog_fsync_baseline.py --dry-run
python3 run_nvlog_fsync_baseline.py --confirm-destroy \
  --devices sata --workloads seqwrite-4k --njobs 1
python3 run_nvlog_fsync_baseline.py --confirm-destroy
```

Document `tail -f results/<run>/runner.log` and `tail -f results/<run>/logs/serial-*.log` as progress views.

- [ ] **Step 6: Run static and unit verification**

Run: `python3 -m py_compile run_nvlog_fsync_baseline.py test_nvlog_fsync_baseline.py && bash -n nvlog_fsync_container.sh && sh -n nvlog_fsync_guest.sh && python3 -m unittest -v test_nvlog_fsync_baseline.py`

Expected: all checks pass.

- [ ] **Step 7: Verify the exact dry-run matrix**

Run: `python3 run_nvlog_fsync_baseline.py --dry-run > /tmp/nvlog-fsync-plan.json`

Expected: valid JSON with `point_count=36`, `mode=fsync` for every point, 18 SATA points, and 18 NVMe points.

- [ ] **Step 8: Commit Task 4 before destructive execution**

```bash
git add test/aster-ext4-baseline-test/run_nvlog_fsync_baseline.py test/aster-ext4-baseline-test/test_nvlog_fsync_baseline.py test/aster-ext4-baseline-test/README.md
git commit -m "test: run NVLog fsync baseline matrix"
```

- [ ] **Step 9: Run the destructive smoke point**

Run: `python3 run_nvlog_fsync_baseline.py --confirm-destroy --devices sata --workloads seqwrite-4k --njobs 1`

Expected: one valid `summary.csv` row, fio error zero, drain timing present, no kernel-failure signature, and QEMU exit status zero.

- [ ] **Step 10: Start the full matrix after smoke passes**

Run: `nohup python3 run_nvlog_fsync_baseline.py --confirm-destroy > nvlog-fsync-launch.log 2>&1 &`

Expected: a new timestamped result directory with 36 serial points executing strictly one at a time.
