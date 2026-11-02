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
guest. This task does not run the real mode.

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
one four-queue VM invocation and the host-hctx mechanism gate.

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
