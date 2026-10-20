#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Extract unified EXT4/rNVPCe pipeline metrics from serial logs."""

import argparse
import csv
import math
import re
from pathlib import Path


METRIC_RE = re.compile(
    r"\[PIPELINE\] name=(\S+) count=(\d+) total_ns=(\d+) max_ns=(\d+) "
    r"units=(\d+) bins=\[([0-9,]+)\]"
)


def percentile_ns(bins, count, percentile):
    if count == 0:
        return 0
    threshold = math.ceil(count * percentile)
    seen = 0
    for index, samples in enumerate(bins):
        seen += samples
        if seen >= threshold:
            return 1 << index
    raise ValueError("histogram count does not match metric count")


def load_fio_rows(artifacts):
    path = artifacts / "results.csv"
    if not path.exists():
        return {}
    with path.open(newline="") as source:
        return {row["case"]: row for row in csv.DictReader(source)}


def extract_metrics(path):
    metrics = {}
    for match in METRIC_RE.finditer(path.read_text(errors="replace")):
        name, count, total_ns, max_ns, units, bins = match.groups()
        bins = [int(value) for value in bins.split(",")]
        count = int(count)
        if sum(bins) != count:
            raise ValueError(f"{path}: {name} histogram has {sum(bins)} of {count} samples")
        metrics[name] = {
            "count": count,
            "total_ns": int(total_ns),
            "max_ns": int(max_ns),
            "units": int(units),
            "p50_ns": percentile_ns(bins, count, 0.50),
            "p95_ns": percentile_ns(bins, count, 0.95),
            "p99_ns": percentile_ns(bins, count, 0.99),
        }
    return metrics


def seconds(metrics, name):
    return metrics.get(name, {}).get("total_ns", 0) / 1e9


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    artifacts = args.artifacts.resolve()
    fio = load_fio_rows(artifacts)
    detail_rows = []
    stage_rows = []
    for path in sorted(artifacts.glob("*-j*-*m.log")):
        case = path.stem
        metrics = extract_metrics(path)
        if not metrics:
            continue
        for name, values in sorted(metrics.items()):
            detail_rows.append({"case": case, "metric": name, **values})
        is_nv = case.startswith("rnvpce-")
        prefix = "nv" if is_nv else "native"
        fio_row = fio.get(case, {})
        stage_rows.append({
            "case": case,
            "fio_mib_s": fio_row.get("fio_mib_s", ""),
            "drained_mib_s": fio_row.get("drained_mib_s", ""),
            "foreground_s": seconds(metrics, f"{prefix}_foreground_total"),
            "foreground_body_s": seconds(
                metrics, "nv_foreground_persist" if is_nv else "native_foreground_body"
            ),
            "capacity_wait_s": seconds(
                metrics, "nv_capacity_wait" if is_nv else "native_pressure_wait"
            ),
            "flush_s": seconds(metrics, f"{prefix}_flush_total"),
            "io_ready_wait_s": seconds(metrics, "nv_async_ready_wait") if is_nv else "",
            "completion_queue_wait_s": (
                seconds(metrics, "nv_completion_queue_wait") if is_nv else ""
            ),
            "completion_s": seconds(
                metrics, "nv_completion_total" if is_nv else "native_completion_wait"
            ),
            "device_flush_s": seconds(metrics, "nv_device_flush") if is_nv else "",
            "extent_publish_s": seconds(metrics, "nv_extent_publish") if is_nv else "",
            "reclaim_s": seconds(metrics, "nv_reclaim") if is_nv else "",
        })
    if not detail_rows:
        raise SystemExit("no [PIPELINE] metrics found")
    for name, rows in [("pipeline-metrics.csv", detail_rows), ("pipeline-stages.csv", stage_rows)]:
        with (artifacts / name).open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)
    print("case,fio_MiB/s,drained_MiB/s,foreground_s,capacity_wait_s,flush_s,completion_s")
    for row in stage_rows:
        print(
            f"{row['case']},{row['fio_mib_s']},{row['drained_mib_s']},"
            f"{row['foreground_s']:.6f},{row['capacity_wait_s']:.6f},"
            f"{row['flush_s']:.6f},{row['completion_s']:.6f}"
        )


if __name__ == "__main__":
    main()
