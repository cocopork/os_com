#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Aggregates a complete mixed-sync experiment matrix."""

import csv
from pathlib import Path
import statistics
import sys

import matrix
import parse_result


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


def expected_cases(modes: tuple[str, ...]) -> set[str]:
    return {
        matrix.Point(device, mode, read_percent, sync_percent, repeat).case
        for repeat in matrix.REPEATS
        for device in matrix.DEVICES
        for mode in modes
        for read_percent in matrix.READ_PERCENTAGES
        for sync_percent in matrix.SYNC_PERCENTAGES
    }


def validate_complete(rows: list[dict], modes: tuple[str, ...] = matrix.MODES) -> None:
    expected = expected_cases(modes)
    actual = {row["case"] for row in rows}
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    if len(actual) != len(rows):
        raise ValueError("duplicate result cases")
    if missing or unexpected:
        raise ValueError(
            f"missing {len(missing)} cases and found {len(unexpected)} unexpected cases"
        )


def aggregate(rows: list[dict], modes: tuple[str, ...] = matrix.MODES) -> list[dict]:
    validate_complete(rows, modes)
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
    expected_coordinates = (
        len(matrix.DEVICES)
        * len(modes)
        * len(matrix.READ_PERCENTAGES)
        * len(matrix.SYNC_PERCENTAGES)
    )
    if len(output) != expected_coordinates:
        raise ValueError(
            f"expected {expected_coordinates} aggregate coordinates, found {len(output)}"
        )
    return output


def normalize_nvlog_result(payload: dict) -> dict:
    for field in (
        "benchmark_exit",
        "sync_exit",
        "unmount_exit",
        "qemu_exit",
        "e2fsck_exit",
    ):
        if payload.get(field) != 0:
            raise ValueError(f"NVLog result has non-zero {field}")
    if payload.get("parse_valid") is not True or payload.get("mode") != "nvlog":
        raise ValueError("NVLog result is not a valid parsed NVLog point")

    operations = int(payload["operations"])
    reads = int(payload["reads"])
    writes = int(payload["writes"])
    fsyncs = int(payload["fsyncs"])
    file_bytes = int(payload["file_bytes"])
    files_opened = int(payload["files_opened"])
    prewrite_bytes = file_bytes * files_opened
    mib = 1024 * 1024
    return {
        "case": payload["case"],
        "device": payload["device"],
        "mode": payload["mode"],
        "read_percent": int(payload["read_percent"]),
        "write_percent": int(payload["write_percent"]),
        "sync_percent": int(payload["sync_percent"]),
        "repeat": int(payload["repeat"]),
        "threads": int(payload["threads"]),
        "file_mib": file_bytes // mib,
        "dataset_mib": prewrite_bytes // mib,
        "configured_dram_cache_mib": int(payload["configured_dram_cache_mib"]),
        "configured_nv_data_cache_mib": int(payload["configured_nv_data_cache_mib"]),
        "configured_total_data_cache_mib": int(payload["configured_total_data_cache_mib"]),
        "cache_control": payload["cache_control"],
        "prewrite_bytes": prewrite_bytes,
        "operations": operations,
        "reads": reads,
        "writes": writes,
        "fsyncs": fsyncs,
        "actual_read_percent": reads * 100 / operations if operations else 0.0,
        "actual_sync_percent": fsyncs * 100 / writes if writes else 0.0,
        "elapsed_seconds": float(payload["elapsed_seconds"]),
        "operations_per_second": float(payload["operations_per_second"]),
        "mib_per_second": float(payload["mib_per_second"]),
        "latency_overflow_count": int(payload["latency_overflow_count"]),
        "mean_latency_us": float(payload["mean_latency_us"]),
        "p50_latency_us": float(payload["p50_latency_us"]),
        "p95_latency_us": float(payload["p95_latency_us"]),
        "p99_latency_us": float(payload["p99_latency_us"]),
        "p999_latency_us": float(payload["p999_latency_us"]),
    }


def write_csv(path: Path, rows: list[dict]) -> None:
    if not rows:
        raise ValueError(f"refusing to write empty CSV: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def load_results(root: Path) -> list[dict]:
    rows = [
        parse_result.validate_saved_result(root / point.case, point)
        for point in matrix.matrix()
    ]
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
