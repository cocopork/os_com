# Test Harness Guide

This directory contains host-side Asterinas EXT4 harnesses. Run each specialized harness from its own directory; this guide defines the shared environment and acceptance policy.

- [FIO baseline and recovery](aster-ext4-baseline-test/README.md)
- [Preliminary EXT4/rNVPC/rNVPCe performance sweep](preliminary-performance-sweep/README.md)
- [EXT4 journal comparison](ext4-journal-compare/README.md)
- [Filebench macro workloads](filebench-test/README.md)
- [SQLite macro workload](sqlite-test/README.md)
- [Combined macro runner](macro-tests/README.md)

## Shared Environment

For subsequent os_com rNVPCe/rTIO performance tests, keep
`rtio_cpu_isolation=on` in `--rnvpc-kcmd` and use at least64vCPU (current
baseline64). Keep nice0, `rtio_cpu_trace` off, and latency counters off for
throughput runs. Diagnostic latency runs must use the same CPU isolation.
This standing performance policy does not enable isolation in small-vCPU ktests.
Do not compare historical unisolated runs as a controlled before/after pair.
See [CPU placement and results](aster-ext4-baseline-test/results/20261013-cpu-isolation/README.md).

Run host entry points on a KVM-capable host with Docker access. The harnesses enter their own privileged container and pass through the selected block device; do not run guest scripts directly. Use one build at a time. A baseline rerun may use `--skip-build` only when its build stamp confirms that both the kernel tree and installed guest script are unchanged.

Each result run needs exclusive ownership of its target device and QEMU instance. Use repository-local images and result directories only; do not share images, result directories, container caches, or devices with `asterinas_ext4`, `research1`, or another active experiment. The launchers reject mounted or actively used devices, but that check is not a substitute for coordination.

## Measurement Policy

Keep latency or tracing instrumentation off for absolute-throughput runs. Enable it only for a separately labeled diagnostic run, then do not compare its throughput with an uninstrumented result. Preserve the guest completion marker, configuration snapshot, and parser output for every candidate point. A timeout, silent guest exit, missing completion marker, invalid parser row, or changed configuration excludes that point from formal comparison.

Unless an experiment explicitly states that SSD allocation metadata is part of
the measured mechanism, prepare the target data before measurement as a
low-fragmentation, logically and physically contiguous file. The default
multi-job workload uses non-overlapping ranges of that one file. If the purpose
requires one inode per job, create the files sequentially, never concurrently,
and verify that their extent counts remain low and comparable. Concurrent file
creation changes the extent-tree shape and is a separate workload variable.

Record the prepared file size, data-extent count and extent-index-block count
from a read-only `debugfs stat` (or an equivalent structured inspector) for
every point. A point with an unexplained layout difference cannot be used for
thread or block-size scaling conclusions. Prefer cloning one frozen prepared
filesystem state across a sweep when the harness supports it.

Only an experiment named and documented as `first-write`, `fresh-file`, or
`allocation-included` may create holes/files and allocate SSD blocks, bitmaps,
GDT state or extent nodes during the timed interval. Its results must not be
mixed into the default overwrite curve.

Device-backed harnesses reformat their selected test filesystem. Pass `--confirm-destroy` only after verifying the device mapping and that no needed data is present. Formatting is intentional and destructive.

## Acceptance

Formal throughput uses `busy_bw_mib_s` only. Drain timing is diagnostic for tail completion and writeback behavior. An accepted result must include repeated comparable observations; exact workload, device, VM resources, code revision and dirty state, instrumentation state, command, completion evidence, and final summary. The accepted package is recorded in [`../EXPERIMENTS.md`](../EXPERIMENTS.md); raw serial, QEMU, build, and console logs are not evidence-package assets.
