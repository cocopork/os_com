#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Aggregates validated full end-to-end sweep results."""

import csv
import json
from pathlib import Path
import statistics
import sys


KEYS = ("device", "mode", "workload", "axis", "value", "bs_kib", "jobs")
METRICS = ("fio_mib_s", "including_sync_mib_s", "mean_latency_us", "p99_latency_us")
RESULT_COLUMNS = (
    "case", "device", "mode", "workload", "axis", "value", "bs_kib",
    "jobs", "repeat", "actual_mib", "fio_mib_s", "including_sync_mib_s",
    "iops", "mean_latency_us", "p99_latency_us",
)


def normalize_result(payload: dict) -> dict:
    """Project a result onto the stable common fio CSV schema."""
    missing = [field for field in RESULT_COLUMNS if field not in payload]
    if missing:
        raise ValueError(f"result is missing fields: {', '.join(missing)}")
    return {field: payload[field] for field in RESULT_COLUMNS}


def normalize_nvlog_result(payload: dict) -> dict:
    """Project a validated NVLog result onto the common fio CSV schema."""
    if payload.get("mode") != "nvlog":
        raise ValueError("expected an NVLog result")
    return normalize_result(payload)


def aggregate(rows: list[dict]) -> list[dict]:
    groups = {}
    for row in rows:
        groups.setdefault(tuple(row[key] for key in KEYS), []).append(row)
    result = []
    for key, samples in sorted(groups.items()):
        output = dict(zip(KEYS, key))
        output["samples"] = len(samples)
        for metric in METRICS:
            values = [
                float(row[metric])
                for row in samples
                if row.get(metric) not in (None, "")
            ]
            if not values:
                for suffix in ("mean", "stddev", "min", "max"):
                    output[f"{metric}_{suffix}"] = None
                continue
            output[f"{metric}_mean"] = statistics.mean(values)
            output[f"{metric}_stddev"] = statistics.pstdev(values)
            output[f"{metric}_min"] = min(values)
            output[f"{metric}_max"] = max(values)
        result.append(output)
    return result


def write_csv(path: Path, rows: list[dict]) -> None:
    if not rows:
        return
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def main() -> None:
    root = Path(sys.argv[1])
    rows = [
        normalize_result(json.loads(path.read_text()))
        for path in sorted(root.glob("*/result.json"))
    ]
    write_csv(root / "results.csv", rows)
    write_csv(root / "aggregate.csv", aggregate(rows))
    print(f"validated {len(rows)}/648 runs, aggregated {len(aggregate(rows))}/216 points")


if __name__ == "__main__":
    main()
