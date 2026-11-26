#!/usr/bin/env python3
"""Aggregates the reconfigured append, overwrite, and cold-read thread sweep."""

import csv
import json
from pathlib import Path
import statistics


ROOT = Path(__file__).resolve().parent.parent
KEYS = ("device", "mode", "workload", "jobs")
METRICS = ("fio_mib_s", "iops", "mean_latency_us", "p99_latency_us")
RESULT_FIELDS = (
    "case", "device", "mode", "workload", "axis", "value", "bs_kib",
    "jobs", "repeat", "actual_mib", "fio_mib_s", "including_sync_mib_s",
    "iops", "mean_latency_us", "p99_latency_us",
)


def main() -> None:
    rows = [
        json.loads(path.read_text())
        for path in sorted(ROOT.glob("raw/*/result.json"))
    ]
    if len(rows) != 486:
        raise SystemExit(f"expected 486 results, found {len(rows)}")

    with (ROOT / "results.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=RESULT_FIELDS, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)

    groups = {}
    for row in rows:
        groups.setdefault(tuple(row[key] for key in KEYS), []).append(row)
    aggregate = []
    for key, samples in sorted(groups.items()):
        if len(samples) != 3:
            raise SystemExit(f"expected 3 repetitions for {key}, found {len(samples)}")
        result = dict(zip(KEYS, key))
        result["samples"] = len(samples)
        for metric in METRICS:
            values = [float(sample[metric]) for sample in samples]
            result[f"{metric}_mean"] = statistics.mean(values)
            result[f"{metric}_stddev"] = statistics.pstdev(values)
            result[f"{metric}_min"] = min(values)
            result[f"{metric}_max"] = max(values)
        aggregate.append(result)

    with (ROOT / "aggregate.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(aggregate[0]))
        writer.writeheader()
        writer.writerows(aggregate)
    print(f"aggregated {len(rows)} runs into {len(aggregate)} coordinates")


if __name__ == "__main__":
    main()
