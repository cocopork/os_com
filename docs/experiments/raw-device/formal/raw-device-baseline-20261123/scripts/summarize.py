#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Collects raw-device result files and writes repetition summaries."""

import csv
import json
from pathlib import Path
import statistics
import sys


ROW_FIELDS = (
    "case",
    "device",
    "system",
    "workload",
    "axis",
    "bs_kib",
    "jobs",
    "repeat",
    "actual_mib",
    "virtqueues",
    "fio_mib_s",
    "iops",
    "mean_latency_us",
    "p99_latency_us",
)
GROUP_FIELDS = ("device", "system", "workload", "axis", "bs_kib", "jobs", "virtqueues")
METRICS = ("fio_mib_s", "iops", "mean_latency_us", "p99_latency_us")


def collect_rows(root: Path) -> list[dict]:
    rows = []
    for result in sorted(root.glob("raw-*/result.json")):
        rows.extend(json.loads(result.read_text()))
    return rows


def aggregate_rows(rows: list[dict]) -> list[dict]:
    groups = {}
    for row in rows:
        key = tuple(row[field] for field in GROUP_FIELDS)
        groups.setdefault(key, []).append(row)
    aggregated = []
    for key, samples in sorted(groups.items()):
        record = dict(zip(GROUP_FIELDS, key))
        record["samples"] = len(samples)
        for metric in METRICS:
            values = [float(sample[metric]) for sample in samples]
            record[f"{metric}_mean"] = statistics.fmean(values)
            record[f"{metric}_stddev"] = statistics.stdev(values) if len(values) > 1 else 0.0
            record[f"{metric}_min"] = min(values)
            record[f"{metric}_max"] = max(values)
        aggregated.append(record)
    return aggregated


def write_csv(path: Path, rows: list[dict], fields) -> None:
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def main() -> None:
    root = Path(sys.argv[1])
    rows = collect_rows(root)
    aggregate = aggregate_rows(rows)
    aggregate_fields = (*GROUP_FIELDS, "samples") + tuple(
        f"{metric}_{suffix}"
        for metric in METRICS
        for suffix in ("mean", "stddev", "min", "max")
    )
    write_csv(root / "results.csv", rows, ROW_FIELDS)
    write_csv(root / "aggregate.csv", aggregate, aggregate_fields)
    print(f"collected {len(rows)} runs from {len(aggregate)} points")


if __name__ == "__main__":
    main()
