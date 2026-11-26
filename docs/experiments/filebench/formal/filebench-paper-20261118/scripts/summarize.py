#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import csv
import statistics
import sys
from pathlib import Path


def main():
    root = Path(sys.argv[1])
    rows = list(csv.DictReader((root / "raw-results.csv").open()))
    groups = {}
    for row in rows:
        groups.setdefault((row["device"], row["mode"], row["workload"]), []).append(row)
    columns = [
        "device", "mode", "workload", "repeats", "ops_s_median", "ops_s_mean",
        "ops_s_min", "ops_s_max", "mb_s_median", "latency_ms_median",
    ]
    summary = []
    for key, samples in sorted(groups.items()):
        ops = [float(row["ops_s"]) for row in samples]
        mb = [float(row["mb_s"]) for row in samples]
        latency = [float(row["latency_ms"]) for row in samples]
        summary.append(dict(
            device=key[0], mode=key[1], workload=key[2], repeats=len(samples),
            ops_s_median=f"{statistics.median(ops):.3f}",
            ops_s_mean=f"{statistics.mean(ops):.3f}",
            ops_s_min=f"{min(ops):.3f}", ops_s_max=f"{max(ops):.3f}",
            mb_s_median=f"{statistics.median(mb):.3f}",
            latency_ms_median=f"{statistics.median(latency):.6f}",
        ))
    with (root / "summary.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=columns)
        writer.writeheader()
        writer.writerows(summary)
    for row in summary:
        print(
            f'{row["device"]:4s} {row["mode"]:6s} {row["workload"]:17s} '
            f'{row["ops_s_median"]:>12s} ops/s {row["mb_s_median"]:>10s} MB/s'
        )


if __name__ == "__main__":
    main()
