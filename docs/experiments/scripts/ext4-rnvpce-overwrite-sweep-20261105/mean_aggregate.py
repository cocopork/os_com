#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Aggregate five complete overwrite sweeps with arithmetic means."""

import argparse
import csv
from collections import defaultdict
from pathlib import Path
from statistics import fmean


IDENTITY_FIELDS = (
    "case",
    "mode",
    "axis",
    "workload",
    "value",
    "bs_kib",
    "jobs",
    "actual_mib",
)
METRIC_FIELDS = (
    "fio_mib_s",
    "including_sync_mib_s",
    "iops",
    "mean_latency_us",
    "p99_latency_us",
)


def read_rows(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        return list(csv.DictReader(source))


def average_samples(rows: list[dict], expected_rounds: int) -> dict[str, dict]:
    grouped = defaultdict(list)
    for row in rows:
        grouped[row["case"]].append(row)
    averaged = {}
    for case, samples in grouped.items():
        rounds = {sample["round"] for sample in samples}
        if len(samples) != expected_rounds or len(rounds) != expected_rounds:
            raise ValueError(f"{case}: expected {expected_rounds} repeats")
        first = samples[0]
        if any(
            sample.get(field) != first.get(field)
            for sample in samples[1:]
            for field in IDENTITY_FIELDS
        ):
            raise ValueError(f"{case}: identity changed across repeats")
        result = {field: first[field] for field in IDENTITY_FIELDS}
        for field in METRIC_FIELDS:
            result[field] = format(fmean(float(sample[field]) for sample in samples), ".15g")
        result["sample_count"] = str(expected_rounds)
        result["statistic"] = "arithmetic_mean"
        averaged[case] = result
    return averaged


def aggregate(
    repeats: list[tuple[str, Path]],
    raw_output: Path,
    mean_output: Path,
) -> tuple[int, int]:
    raw_rows = []
    for round_name, path in repeats:
        for row in read_rows(path):
            item = dict(row)
            item["round"] = round_name
            raw_rows.append(item)
    averaged = average_samples(raw_rows, expected_rounds=len(repeats))
    raw_output.parent.mkdir(parents=True, exist_ok=True)
    with raw_output.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(raw_rows[0]))
        writer.writeheader()
        writer.writerows(raw_rows)
    mean_fields = list(IDENTITY_FIELDS + METRIC_FIELDS) + ["sample_count", "statistic"]
    with mean_output.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=mean_fields)
        writer.writeheader()
        writer.writerows(averaged[case] for case in sorted(averaged))
    return len(raw_rows), len(averaged)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw_output", type=Path)
    parser.add_argument("mean_output", type=Path)
    parser.add_argument(
        "--repeat",
        action="append",
        required=True,
        metavar="ROUND:CSV",
        help="repeat round name and results.csv path",
    )
    args = parser.parse_args()
    repeats = []
    for value in args.repeat:
        round_name, separator, path = value.partition(":")
        if not separator or not round_name or not path:
            parser.error(f"invalid --repeat value: {value}")
        repeats.append((round_name, Path(path)))
    raw_count, mean_count = aggregate(repeats, args.raw_output, args.mean_output)
    print(f"wrote {raw_count} raw samples and {mean_count} arithmetic means")


if __name__ == "__main__":
    main()
