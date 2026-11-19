#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Aggregates a complete mixed-sync experiment matrix."""

import csv
import json
from pathlib import Path
import statistics
import sys

import matrix


KEYS = ("device", "mode", "read_percent", "write_percent", "sync_percent")
METRICS = (
    "operations_per_second",
    "mib_per_second",
    "mean_latency_us",
    "p50_latency_us",
    "p95_latency_us",
    "p99_latency_us",
    "p999_latency_us",
    "actual_read_percent",
    "actual_sync_percent",
)


def validate_complete(rows: list[dict]) -> None:
    expected = {point.case for point in matrix.matrix()}
    actual = {row["case"] for row in rows}
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    if len(actual) != len(rows):
        raise ValueError("duplicate result cases")
    if missing or unexpected:
        raise ValueError(
            f"missing {len(missing)} cases and found {len(unexpected)} unexpected cases"
        )


def aggregate(rows: list[dict]) -> list[dict]:
    validate_complete(rows)
    groups: dict[tuple, list[dict]] = {}
    for row in rows:
        groups.setdefault(tuple(row[key] for key in KEYS), []).append(row)
    output = []
    for key, samples in sorted(groups.items()):
        if len(samples) != 3 or {int(sample["repeat"]) for sample in samples} != {1, 2, 3}:
            raise ValueError(f"coordinate {key!r} does not contain repeats 1, 2, and 3")
        result = dict(zip(KEYS, key))
        result["samples"] = len(samples)
        for metric in METRICS:
            values = [float(sample[metric]) for sample in samples]
            result[f"{metric}_mean"] = statistics.mean(values)
            result[f"{metric}_stddev"] = statistics.pstdev(values)
            result[f"{metric}_min"] = min(values)
            result[f"{metric}_max"] = max(values)
        output.append(result)
    if len(output) != 144:
        raise ValueError(f"expected 144 aggregate coordinates, found {len(output)}")
    return output


def write_csv(path: Path, rows: list[dict]) -> None:
    if not rows:
        raise ValueError(f"refusing to write empty CSV: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def load_results(root: Path) -> list[dict]:
    rows = [json.loads(path.read_text()) for path in sorted(root.glob("*/result.json"))]
    validate_complete(rows)
    return rows


def main() -> None:
    root = Path(sys.argv[1])
    rows = load_results(root)
    aggregated = aggregate(rows)
    write_csv(root / "results.csv", rows)
    write_csv(root / "aggregate.csv", aggregated)
    print(f"validated {len(rows)}/432 runs, aggregated {len(aggregated)}/144 points")


if __name__ == "__main__":
    main()
