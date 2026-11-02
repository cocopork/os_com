# rTIO VirtIO Multi-queue Validation — 2026-11-02

## Verdict

The queue-affine mechanism passed the real-device correctness and hardware-queue
gates. All eight 2 GiB points completed fio, sync, unmount and read-only
`e2fsck -fn` successfully. Every four-worker point submitted on four distinct
virtqueues and used NVMe hctx 0–3 with a largest observed share below 32%.
All queues drained and each ready durability epoch issued one device FLUSH.

Performance acceptance **failed**. Random writes improved, but sequential
including-sync throughput regressed 46.9% with one job and 42.3% with 16 jobs.
The 16-job sequential arm also increased cumulative submission and capacity-wait
time relative to the previous four-worker/single-queue diagnostic. These
results support keeping the multi-worker path experimental and the production
default at one worker. The measured candidate at `936722960` still uses the
queue-aware default; the policy correction is a separate follow-up, not a
change to the measured binary.

## Method and provenance

- Source: `936722960e29285bcf80c732580535f5a7ed85d3`; one temporary build diff
  set `pipeline_profile::ENABLED` from `false` to `true`. It was saved as
  `build-source.diff` and restored immediately after copying the release ISO.
  No other kernel source changed for measurement; no rebuild ran during it.
- Release kernel check and build both exited 0. This is an instrumented release
  experiment, with one sample per point, not an uninstrumented performance
  result or a statistical confidence interval.
- One ISO for all eight points, SHA-256:
  `fbb3259fe8322138ead265868e930dfd0eb7e468400db4e5b0ae1e52cf553e35`.
- Tools: `os-com-recovery-tools:workspace-20261018`, image ID
  `sha256:6a1ae86b819913bdb9e39b0a8b47fc802ea73aed93e2f5d6bc7f9839d56d4254`;
  container QEMU 10.2.1. Host QEMU 8.2.2 was not used.
- Host: Linux `6.8.0-147-generic`, x86-64; DAPUSTOR
  `DPRP5104T0TG03T8000`, firmware `FC003126`, scheduler `none`.
- Only `/dev/nvme1n1p1`, exact 274,877,906,944 bytes, major:minor `259:6`,
  parent `nvme1n1` (`259:2`), was exposed. It was reformatted before each
  measured point as 4 KiB no-journal EXT4. p2, p3 and the whole disk were not
  written. Target identity, mounts, swap, holders, descendants and host-root
  open-user checks ran immediately before formatting and QEMU exposure.
- VM: KVM, q35, 64 vCPU, 8 GiB RAM, OVMF, raw target with `cache=none`,
  `aio=threads`, virtqueue depth 1024. Request merging, discard, write-zeroes,
  event index and indirect descriptors disabled for the target.
- rNVPCe: 128 MiB NVM pool, 64 MiB metadata reserve, 64 MiB data limit,
  simulated persistence `fast`, CPU isolation and tracing enabled. The ISO
  has no worker override: the measured candidate selects 1 worker for 1 queue
  and 4 workers for 4 queues. Native/WAL mounts remain single-worker.
- fio: buffered `psync`, depth 1 per thread, CPU set 0–31, exactly 2 GiB total,
  1 or 16 jobs, fixed random seed. Random points first prepare files using a
  direct sequential write on native EXT4; the measured pass is the first
  rNVPCe random pass over those existing SSD-backed files. Sequential points
  write new files. Each point begins with a fresh filesystem.
- The controls ran first, then the four-queue arm, serialized. The guest's
  including-sync interval spans fio start through final sync, before unmount;
  profiling/serial overhead inside that interval is included.

Raw evidence is under
`/home/zn/asterinas-build-env/rtio-virtio-multiqueue-20261102/`, outside Git.
Each `q{1,4}-{randwrite-4k,write-128k}-j{1,16}` directory contains the ISO and
input hashes, QEMU log and status, mkfs/e2fsck logs, complete pipeline snapshots,
QMP replies, IOThread TIDs, affinity, sysfs queue map, probe source and preflight,
raw trace, final hctx counts, root-helper evidence and parsed `result.json`.

## Throughput

All rates are MiB/s. Change compares including-sync throughput, 4 queues / 1 queue.

| Workload | Jobs | 1q fio | 4q fio | 1q including sync | 4q including sync | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| randwrite-4k | 1 | 204.15 | 267.96 | 189.28 | 246.45 | +30.2% |
| randwrite-4k | 16 | 199.12 | 287.64 | 185.51 | 253.47 | +36.6% |
| write-128k | 1 | 2409.41 | 1030.70 | 1312.82 | 696.60 | -46.9% |
| write-128k | 16 | 801.88 | 400.78 | 607.72 | 350.68 | -42.3% |

## Latency and pipeline counters

Mean is fio total write latency; p99 is fio completion latency, both in µs.
Submission and capacity-wait columns are cumulative seconds across callers,
not elapsed wall time. Epoch/FLUSH counts are equal at the final snapshot.

| Point | Mean µs | p99 µs | `nv_data_submit` s | Capacity wait s | Epochs / FLUSHes | Peak in-flight pages |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| q1-randwrite-4k-j1 | 18.83 | 25.73 | 9.882 | 3.492 | 512 / 512 | 2048 |
| q1-randwrite-4k-j16 | 310.09 | 8978.43 | 9.961 | 73.037 | 512 / 512 | 2048 |
| q1-write-128k-j1 | 51.57 | 134.14 | 0.655 | 0.037 | 512 / 512 | 2048 |
| q1-write-128k-j16 | 2473.16 | 8454.14 | 1.021 | 2.186 | 507 / 507 | 6144 |
| q4-randwrite-4k-j1 | 14.23 | 28.29 | 14.391 | 0.854 | 508 / 508 | 6144 |
| q4-randwrite-4k-j16 | 213.55 | 514.05 | 15.591 | 13.653 | 561 / 561 | 16850 |
| q4-write-128k-j1 | 120.92 | 329.73 | 3.330 | 0.639 | 227 / 227 | 16384 |
| q4-write-128k-j16 | 4916.78 | 39583.74 | 8.741 | 30.742 | 221 / 221 | 16384 |

For 16-job sequential writes, the previous four-worker/single-queue diagnostic
reported submission 4.74 s and capacity wait 18.16 s; this arm measured 8.74 s
and 30.74 s. The reduction gate therefore fails as well. The historical run
used a different kernel path and is contextual evidence; the same-image 1q
comparison above independently demonstrates the regression. Four-queue
sequential runs create many more ready batches (2,489 at one job, 1,437 at
16 jobs, versus 512 in each control), while sharing fewer FLUSHes (227 and
221). More fragmented migration and accumulated submission delay remain
observations requiring investigation; these measurements do not establish a
specific lock as the cause.

## Queue balance and drain

Slash-separated entries are queue 0/1/2/3 in order; controls have queue 0 only.
Worker BIO counts exclude the final quiesced queue-0 sync drain. Driver counters
include native preparation, metadata and sync I/O. Depth is the maximum sampled
staging depth; descriptor ticks are the driver's raw TSC ticks, not nanoseconds.
Every final queue had staging depth 0 and in-flight requests 0. Every queue
reported one active forwarding thread and peak forwarding count 1.

| Point | Worker submitted BIOs | Worker submitted pages | Max staging depth | Descriptor waits | Descriptor wait ticks | IRQ completions |
| --- | --- | --- | --- | --- | --- | --- |
| q1-randwrite-4k-j1 | 517120 | 517120 | 107 | 0 | 0 | 572525 |
| q1-randwrite-4k-j16 | 517120 | 517120 | 184 | 37 | 11713220 | 575257 |
| q1-write-128k-j1 | 16163 | 517120 | 10 | 0 | 0 | 21495 |
| q1-write-128k-j16 | 16163 | 517120 | 5 | 0 | 0 | 21737 |
| q4-randwrite-4k-j1 | 131072/123904/131072/134144 | 131072/123904/131072/134144 | 195/126/128/212 | 2/18/26/4 | 8567324/5095106/6467220/11769154 | 135603/123952/135342/177763 |
| q4-randwrite-4k-j16 | 122084/127832/134225/133043 | 122084/127832/134225/133043 | 956/1540/1576/1597 | 541/181/330/273 | 630721508/634252876/614614596/204552138 | 140676/141501/147947/144754 |
| q4-write-128k-j1 | 4093/3895/3787/4179 | 130944/124608/121184/133696 | 11/8/3/9 | 0/0/0/0 | 0/0/0/0 | 4962/3912/5967/6369 |
| q4-write-128k-j16 | 3479/4433/4122/4302 | 111328/141632/131904/137664 | 21/11/17/9 | 0/0/0/0 | 0/0/0/0 | 5966/4447/6354/4676 |

All four-worker logs contain worker→queue mappings `0→0, 1→1, 2→2, 3→3`
and nonzero data BIOs/pages from each worker. Full queued/submitted BIO and page
counts are retained in each `result.json`. The final shared-coordinator
snapshots report active workers 0; the four-queue arm reached peak workers 4.
The separate cross-queue fault tests in Task 5 establish failure semantics;
this successful-I/O matrix adds physical-device evidence and does not repeat
fault injection or claim a new full-kernel regression pass.

## Authoritative host hardware-queue evidence

QMP mapped virtqueue `i` only to `iothread<i>`. The selected host CPU/hctx pairs
were `(1,0)`, `(3,1)`, `(5,2)`, `(7,3)`; the control used only `(1,0)`.
The harness recorded QMP host TIDs, applied affinity before resume, and saved
`taskset` output. The typed `kprobe:nvme_queue_rq` probe filtered the parent
request queue and counted `blk_mq_hw_ctx.queue_num`. Every trace observed the
readiness marker, remained alive through guest exit, then exited 0 on SIGINT
with a final map boundary. All QEMU runs exited 33 via the guest debug-exit.

| Point | hctx 0/1/2/3 requests | Other hctx requests | Active hctx | Largest share | Four-hctx gate |
| --- | --- | --- | ---: | ---: | --- |
| q1-randwrite-4k-j1 | 572026/0/0/0 | 48:1, 105:1 | 3 | 100.00% | control; not applied |
| q1-randwrite-4k-j16 | 576520/0/0/0 | 14:1 | 2 | 100.00% | control; not applied |
| q1-write-128k-j1 | 20980/0/0/0 | 6:1, 90:1 | 3 | 99.99% | control; not applied |
| q1-write-128k-j16 | 21218/0/0/0 | 15:1, 87:1 | 3 | 99.99% | control; not applied |
| q4-randwrite-4k-j1 | 135108/123952/135342/177763 | 86:1, 127:1 | 6 | 31.07% | PASS |
| q4-randwrite-4k-j16 | 140648/141832/148329/145122 | 98:1, 124:1 | 6 | 25.75% | PASS |
| q4-write-128k-j1 | 4732/3912/5967/6369 | 32:1, 122:1 | 6 | 30.35% | PASS |
| q4-write-128k-j16 | 5734/4448/6354/4676 | 47:1, 48:1 | 6 | 29.95% | PASS |

The one or two requests on other hctx are consistent with the recorded
read-only readiness reads. The trace does not label requests by phase.
Traces cover the whole guest run, including random-file preparation, metadata,
measured fio and sync; they are not fio-only distributions. In each four-queue
point, hctx 0–3 independently carry thousands of requests, so readiness reads
do not create the observed four-queue result. The 70% gate applies to all traced
requests exactly as specified.

## Harness corrections and verification

The initial preflight attempt stopped before formatting because host psmisc
`fuser -s -- PATH` prints usage and status 1. Its evidence is retained under
`failed-fuser/`. A regression test invoking the real CLI on an open temporary
file failed, then passed after removing `--` from the canonical absolute-path
arguments. The root-helper status/output/identity checks remain strict; the preliminary
host check also rejects diagnostic output with status 1, covered by a failing
then passing fixture.

The first successful control's fio JSON was interrupted inside a key by a
CPU-isolation serial line. Its workload and e2fsck were successful; no rerun
was needed. A failing parser regression was added, then known CPU-isolation
messages and ANSI color codes were stripped while preserving byte-level line
endings. All eight retained raw logs now parse and require exactly 2 GiB,
fio error 0, sync 0, unmount 0 and e2fsck 0. New negative fixtures reject
short writes, fio/sync failures, idle or misrouted workers, outstanding requests,
extra FLUSHes and failed hctx gates.

The harness gained tested one/four-queue selection, matching IOThread objects,
repeated target checks, explicit formatting, a 900-second VM bound, and a
serialized matrix wrapper. A mechanism-only rejection has status 3; correctness
or incomplete capture stops the wrapper. `--self-test`, shell syntax checks,
Python compilation and `git diff --check` passed after these changes. No raw
images, JSON, logs or traces are tracked.

Known limitations retained from Task 6: QMP's deadline covers connection but
not every greeting/reply read; the image digest is recorded while Docker
execution still names a mutable tag. This run's per-point image IDs agree.

## Reproduction

From the worktree, build in the stated container with the recorded profiling
change, then copy/fingerprint the produced ISO and restore that source flag:

```sh
cargo osdk check --release --package aster-kernel
cargo osdk build --release --init-args='/ext2/rebuild-fio.sh' \
  --kcmd-args='console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on rtio_cpu_trace=on'
```

With a caller-owned external artifact parent and the fingerprinted ISO,
`ext2.img` and `exfat.img` under `SOURCE_ART`:

```sh
ROOT_ART=/path/to/new-matrix-root \
SOURCE_ART=/path/to/source \
TOOLS_IMAGE=os-com-recovery-tools:workspace-20261018 \
bash docs/experiments/scripts/rtio-virtio-multiqueue-20261102/run-matrix.sh \
  --authorized-destructive-target=/dev/nvme1n1p1
```

The recorded run split this into the first control, remaining three controls,
and all four arm points using `MATRIX_QUEUES`/`MATRIX_SPECS`, preserving the
failed preflight and parsed successful first-control evidence. Future binaries
with a one-worker default must explicitly select four workers for the arm;
the implicit queue-aware selection above is specific to the measured source.
