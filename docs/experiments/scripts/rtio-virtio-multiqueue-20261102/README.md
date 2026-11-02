# rTIO VirtIO Multiqueue Host Harness

Run the non-destructive fixture checks:

```sh
bash run-validation.sh --self-test
```

The real run requires an explicit named target. Only canonical
`/dev/nvme1n1p1` and `/dev/nvme1n1p2` are accepted. Preflight checks the actual
host major:minor against lsblk, exact 256 GiB size, `nvme1n1` parent, sysfs
holders and block descendants, mounted identities, active swap, and open users.
Before QEMU receives the partition, a scoped privileged helper with the host
PID and user namespaces and read-only host-root bind repeats the open-user
check as host root. It verifies UID 0 maps to host UID 0 with a positive span,
then validates both block nodes against the preflight major:minor identities.
The helper emits one JSON result with the identity checks, device major:minor
values, and `fuser` status/stdout/stderr. It writes no evidence files in the
read-only chroot. Helper exit statuses are 0 for unused, 1 for in-use, and 2
for validation failure; the embedded `fuser` unused status is 1. The host runner
saves Docker stdout as `open-users-root.json`, stderr as
`open-users-root.stderr`, and the exit status as `open-users-root.status`.
It accepts only an unused result with matching identities, empty diagnostics,
and consistent statuses. An unavailable helper or ambiguous result aborts.
The selected partition is passed raw to QEMU and may be overwritten by the
guest. `QUEUES=1` selects one virtqueue/IOThread; `QUEUES=4` (the default)
selects four. Other values are rejected. Guest worker count is independent:
the current kernel defaults to one, and the four-worker arm requires an ISO
with `rtio_workers=4` in its kernel command line.
`FORMAT_TARGET=1` reformats the authorized target as 4 KiB no-journal EXT4.
Both formatting and QEMU exposure repeat the target and host-root open-user
checks. After validating the target identity, the standalone runner acquires
`/run/lock/rtio-mq-<major>-<minor>.lock` and holds it until exit, including
container cleanup and optional read-only e2fsck. This stable lock inode is
shared across artifact roots and is never unlinked on release. A competing
run for the same major:minor fails before artifact creation or device exposure;
different verified partition identities use different locks. Failure to create
or acquire the shared lock aborts. `CHECK_FILESYSTEM=1` enables post-guest
`e2fsck -fn` inside that same lock lifetime; the matrix always enables it and
does not acquire a nested device lock. The host `fuser` CLI is invoked without `--`, which this host's
psmisc version rejects; both target paths are already canonical absolute paths.

```sh
SOURCE_ART=/path/to/input-images \
ART=/path/to/new-evidence-directory \
TOOLS_IMAGE=os-com-recovery-tools:workspace-20261018 \
QEMU_BIN=qemu-system-x86_64 \
bash run-validation.sh --authorized-destructive-target=/dev/nvme1n1p1
```

`SOURCE_ART` must contain `selected.iso`, `ext2.img`, and `exfat.img`. `ART`
must name a directory that does not yet exist under an existing caller-owned
parent; the final directory is created atomically. The ISO must be an image whose
guest workflow exits after its fio run; the Task 7 matrix supplies that image
and checks fio, sync, unmount, and e2fsck results. The runner itself handles
one VM invocation and the host-hctx mechanism gate. The control records the
same trace without applying the four-hctx threshold. Exit 3 specifically means
a completed capture failed that threshold; other nonzero statuses indicate a
runner failure. QEMU has a 900-second timeout.

The runner rejects non-local Docker endpoints, records the local Unix socket
and daemon ID, and compares them again before exposing the device. It records
the Docker image digest, exact QEMU version/device help,
input image hashes, target identity, the sysfs hardware queue map, selected
CPU placements, QMP IOThread IDs and host-visible TIDs, taskset output, kernel
version, the rendered typed probe, probe preflight, raw trace, and parsed hctx
counts. `mapping.json` links guest queue, IOThread ID/TID, host CPU, hctx, and
the source sysfs `cpu_list` path. QEMU uses `--pid=host`; a short privileged
helper applies affinity to those TIDs. A second scoped privileged helper runs
the host `/usr/bin/bpftrace` through `chroot /host` with the host root mounted
read-only. No CPU-only fallback is accepted if the typed probe or symbol fails.

The VM starts paused. QMP `device_add` passes a structured JSON
`iothread-vq-mapping` with virtqueue `i` assigned only to `iothread<i>`; a
dedicated PCIe root port permits that QMP hotplug. The runner saves and pins
all four IOThreads and attaches the trace before QMP `cont` starts guest I/O.
While QEMU remains paused and the tracer is alive, the runner issues read-only
4 KiB direct reads of the authorized target until a 20-second deadline or the
filtered `nvme_queue_rq` handler emits `READY parent=nvme1n1`. Each read's timeout
is bounded by the remaining deadline, with a one-second interval between reads;
there is no attempt-count cap. Attachment banner text and probe count are ignored.
`readiness-reads.json` records attempted and completed read counts, and
`readiness-read-attempts.jsonl` records each attempt's start, timeout, elapsed
time, and exit status. Reads made after probe attachment contribute to
the raw trace and hctx counts; a read missed during attachment remains recorded
in `readiness-reads.json` but cannot appear in the trace.
The tracer must survive until the guest exits. It receives SIGINT for controlled
stop; exit status 0 and a final map boundary are required. Logs, inspect data,
and wait statuses are retained before container removal even on failure.
If any capture fails, the stopped container is retained and its name and
manual `docker logs`/`inspect`/`wait` recovery steps are printed.
`hctx-counts.json` preserves raw hardware-context IDs and counts. Its
`accepted` value requires at least four active hctx and a maximum share of
70% of traced requests. This is only the host mechanism gate; it does not
replace Task 7 correctness and performance acceptance.


## Serialized matrix

The current kernel defaults to one worker after the failed performance gate.
Prepare two images from the same source before starting measurements: the
control uses `rtio_workers=1`, and the arm uses `rtio_workers=4`. Pass each to
its matching queue count; a four-worker image on a one-queue target is rejected
at mount. Build with pipeline profiling enabled to capture diagnostics, record
the profiling diff, and restore it after copying/fingerprinting both images.
Do not rebuild during measurements. Each source directory must contain its
`selected.iso`, `ext2.img`, and `exfat.img`. `run-matrix.sh` injects `guest.sh`
and per-point configuration into private ext2 copies; each ISO must boot
`--init-args=/ext2/rebuild-fio.sh`.

```sh
ROOT_ART=/path/to/existing-external-matrix-directory \
SOURCE_ART=/path/to/worker-1-inputs MATRIX_QUEUES=1 \
TOOLS_IMAGE=os-com-recovery-tools:workspace-20261018 \
bash run-matrix.sh --authorized-destructive-target=/dev/nvme1n1p1

ROOT_ART=/path/to/existing-external-matrix-directory \
SOURCE_ART=/path/to/worker-4-inputs MATRIX_QUEUES=4 \
TOOLS_IMAGE=os-com-recovery-tools:workspace-20261018 \
bash run-matrix.sh --authorized-destructive-target=/dev/nvme1n1p1
```

The retained 2026-11-02 experiment used one older queue-aware ISO without an
override; this historical behavior is not the current default.

The default matrix is queues 1 then 4; each arm runs `randwrite 4k 1`,
`randwrite 4k 16`, `write 128k 1`, and `write 128k 16`, 2 GiB each.
`MATRIX_QUEUES` and newline-separated `MATRIX_SPECS` can select missing points
without overwriting evidence. An artifact lock protects image/configuration
edits in the same root. Independently, the runner device lock excludes all
other roots through the post-guest e2fsck. Each point is formatted after the full target
checks. Random writes first prepare SSD-backed files with a direct sequential
write; sequential points use new files. Both use buffered psync fio, depth 1,
a 128 MiB NVM pool, and 64 MiB data capacity.

The wrapper requires exactly 2 GiB of successful fio writes, successful sync
and unmount, and read-only `e2fsck -fn` status 0. A mechanism rejection is
retained and does not prevent collecting other points; a correctness/capture
failure stops the matrix. `results.py` removes only known CPU-isolation serial
messages that can interrupt fio JSON, and writes throughput, latency, complete
pipeline snapshots, mapping, and hctx counts to each point's `result.json`.
Raw traces include readiness reads, random-file preparation, metadata and sync
traffic as well as measured fio traffic. They are not fio-only distributions.

The source image, raw logs, traces, per-point JSON, and disk images stay outside
Git. The report records acceptance and limitations independently of runner exit
status.
