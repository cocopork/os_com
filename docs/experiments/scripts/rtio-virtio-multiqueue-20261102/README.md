# rTIO VirtIO Multiqueue Host Harness

Run the non-destructive fixture checks:

```sh
bash run-validation.sh --self-test
```

The real run requires an explicit named target. Only canonical
`/dev/nvme1n1p1` and `/dev/nvme1n1p2` are accepted. Preflight checks the actual
host major:minor against lsblk, exact 256 GiB size, `nvme1n1` parent, sysfs
holders and block descendants, mounted identities, active swap, and open users.
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
While QEMU remains paused, one bounded 4 KiB direct read of the authorized
target must cause `READY parent=nvme1n1` from the filtered `nvme_queue_rq`
handler. This readiness request is included in the raw trace and hctx counts.
The tracer must survive until the guest exits. It receives SIGINT for controlled
stop; exit status 0 and a final map boundary are required. Logs, inspect data,
and wait statuses are retained before container removal even on failure.
`hctx-counts.json` preserves raw hardware-context IDs and counts. Its
`accepted` value requires at least four active hctx and a maximum share of
70% of traced requests. This is only the host mechanism gate; it does not
replace Task 7 correctness and performance acceptance.
