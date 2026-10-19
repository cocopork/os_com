# Filebench Macro Harness

## Purpose

Exercise file-level create, append, read, delete, and fsync behavior with Filebench. Results are diagnostic until accepted in [`../../EXPERIMENTS.md`](../../EXPERIMENTS.md).

## Matrix

| Dimension | Values |
| --- | --- |
| Device | `sata` (`/dev/sda4`), `nvme` (`/dev/nvme3n1p2`) |
| Profile | `ext4`, `ext2like` by default; `rnvpc`, `rnvpc_extend` only when explicitly selected |
| Workload | `write-heavy`, `fileserver`, `varmail` |
| Threads | `4`, `8` |

The default matrix has 24 points. `ext4` and `ext2like` are rNVPCe and rNVPC surrogates respectively; neither is a measurement of the corresponding implementation. `summary.csv` records the real profile, display label, and mounted options.

## Prerequisites

Follow the shared [test guide](../README.md). The launcher stages the required Filebench 1.5-alpha3 binary and static `mke2fs` into the guest job. Do not edit benchmark sources while another `make kernel` run is active. The selected device must be unmounted and exclusively owned.

## Command

```bash
bash run-filebench.sh --dry-run
bash run-filebench.sh --confirm-destroy --devices nvme --workloads write-heavy,fileserver --threads 4,8
```

Use `--fs`, `--workloads`, `--threads`, `--runtime`, `--nfiles`, and `--filesize` to reduce or expand the matrix. The default workload duration is 30 seconds and the default point timeout is 600 seconds.

## Output

`results/<timestamp>_filebench-macro/` contains `summary.csv`, `run-config.txt`, and raw launcher/serial logs. Rows retain profile, mounted options, status, operations per second, MB/s, latency, validity, and a raw-tail diagnostic. The guest emits a `FILEBENCH_RESULT` record for a parsed result.

## Acceptance checks

Require a matching run configuration, successful guest result, valid parsed row, and repeated comparable observations. A timeout, missing guest result, partial run, crash, or vanished result is diagnostic only. Use the workload-specific metric; do not convert Filebench output into a formal `busy_bw_mib_s` claim.

## Known limitations

Every point reformats the test filesystem and rebuilds its fileset. Default filesets are small unless explicitly enlarged. The driver, device, and QEMU are serial resources; host contention and enabled tracing invalidate comparison.
