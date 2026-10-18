# SQLite Macro Harness

## Purpose

Run SQLite `speedtest1` to exercise data, metadata, and fsync behavior, with an integrity check after each candidate database run. Results remain diagnostic until accepted in [`../../EXPERIMENTS.md`](../../EXPERIMENTS.md).

## Matrix

| Dimension | Values |
| --- | --- |
| Device | `sata` (`/dev/sda4`), `nvme` (`/dev/nvme3n1p2`) |
| Profile | `ext4`, `ext2like` by default; `rnvpc`, `rnvpc_extend` only when explicitly selected |
| Size | `speedtest1 --size 1000` by default |
| Runs | `1` by default |

The default matrix has four points. `ext4` and `ext2like` are rNVPCe and rNVPC surrogates respectively, not measurements of those implementations. `summary.csv` records the actual profile, display label, and mounted options.

## Prerequisites

Follow the shared [test guide](../README.md). `sqlite-speedtest1`, `sqlite3`, and static `mke2fs` are installed through the benchmark job; no extra guest binary is required. Do not alter benchmark sources while a kernel build is active. The selected device must be unmounted and exclusively owned.

## Command

```bash
bash run-sqlite.sh --dry-run
bash run-sqlite.sh --confirm-destroy --devices nvme --sizes 100,1000 --runs 1
```

Use `--fs`, `--sizes`, `--runs`, and `--timeout` to select a subset. The default point timeout is 2700 seconds.

## Output

`results/<timestamp>_sqlite-macro/` contains `summary.csv`, `run-config.txt`, and raw launcher/serial logs. Rows retain status, exit code, elapsed seconds, integrity result, validity, mounted options, and the terminal crash diagnostic.

## Acceptance checks

Accept an ordinary completed row only when `status=ok`, `valid=1`, and `PRAGMA integrity_check` reports `ok`, with matching configuration and repeated comparable observations. Crashes and partial output remain diagnostic evidence; they must never be reported as successful performance points.

## Known limitations

`speedtest1 --size 1000` is intentionally heavy and can create a database of hundreds of MiB. Asterinas may fail at a known index-creation path; such a crash is a system observation, not a harness repair target. Each point reformats the test filesystem, so this harness does not measure a warm-cache workload.
