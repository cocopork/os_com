#!/usr/bin/env python3
"""Aggregate the focused write-regression experiment and compare old baselines."""

import argparse
import csv
import json
import re
import statistics
from pathlib import Path


def load_current(asterinas_root: Path, linux_root: Path) -> list[dict]:
    rows = []
    for path in sorted(asterinas_root.glob("*/result.json")):
        result = json.loads(path.read_text())
        rows.append({
            "system": {"ext4": "asterinas-ext4", "rnvpce": "achilles"}[result["mode"]],
            **result,
        })
    pattern = re.compile(
        r"linux-ext4-(overwrite-randwrite|append-write)-(\d+)k-j(\d+)-r(\d+)"
    )
    for path in sorted(linux_root.glob("*/result.json")):
        result = json.loads(path.read_text())
        match = pattern.fullmatch(result["case"])
        if not match:
            raise ValueError(f"unexpected Linux case: {result['case']}")
        workload, bs_kib, jobs, repeat = match.groups()
        rows.append({
            "system": "linux-ext4",
            "workload": workload,
            "bs_kib": int(bs_kib),
            "jobs": int(jobs),
            "repeat": int(repeat),
            **result,
        })
    return rows


def load_old_baselines(path: Path) -> dict[tuple, float]:
    old = list(csv.DictReader(path.open()))
    coordinates = (
        ("append-write", 128, 1, "bs"),
        ("overwrite-randwrite", 4, 1, "threads"),
        ("overwrite-randwrite", 4, 16, "threads"),
        ("overwrite-randwrite", 4, 32, "threads"),
    )
    baselines = {}
    for mode, system in (("ext4", "asterinas-ext4"), ("rnvpce", "achilles")):
        for workload, bs_kib, jobs, axis in coordinates:
            values = [
                float(row["fio_mib_s"])
                for row in old
                if row["device"] == "nvme"
                and row["mode"] == mode
                and row["workload"] == workload
                and int(row["bs_kib"]) == bs_kib
                and int(row["jobs"]) == jobs
                and row["axis"] == axis
            ]
            if values:
                baselines[(system, workload, bs_kib, jobs)] = statistics.mean(values)
    return baselines


def write_csv(path: Path, rows: list[dict], fields: list[str]) -> None:
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        writer.writerows({field: row.get(field) for field in fields} for row in rows)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("asterinas_root", type=Path)
    parser.add_argument("linux_root", type=Path)
    parser.add_argument("old_results", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    rows = load_current(args.asterinas_root, args.linux_root)
    if len(rows) != 45:
        raise ValueError(f"expected 45 runs, found {len(rows)}")
    baselines = load_old_baselines(args.old_results)
    grouped = {}
    for row in rows:
        key = (row["system"], row["workload"], row["bs_kib"], row["jobs"])
        grouped.setdefault(key, []).append(row)

    summary = []
    for key, runs in sorted(grouped.items()):
        fio = [float(run["fio_mib_s"]) for run in runs]
        sync = [float(run["including_sync_mib_s"]) for run in runs]
        baseline = baselines.get(key)
        mean = statistics.mean(fio)
        summary.append({
            "system": key[0],
            "workload": key[1],
            "bs_kib": key[2],
            "jobs": key[3],
            "runs": len(runs),
            "fio_mib_s_mean": mean,
            "fio_mib_s_stdev": statistics.stdev(fio),
            "including_sync_mib_s_mean": statistics.mean(sync),
            "mean_latency_us": statistics.mean(float(run["mean_latency_us"]) for run in runs),
            "p99_latency_us": statistics.mean(float(run["p99_latency_us"]) for run in runs),
            "old_fio_mib_s_mean": baseline,
            "change_percent": None if baseline is None else (mean / baseline - 1) * 100,
        })

    args.output.mkdir(parents=True, exist_ok=True)
    write_csv(
        args.output / "results.csv",
        rows,
        [
            "system", "case", "workload", "bs_kib", "jobs", "repeat",
            "fio_mib_s", "including_sync_mib_s", "iops", "mean_latency_us",
            "p99_latency_us",
        ],
    )
    write_csv(
        args.output / "summary.csv",
        summary,
        [
            "system", "workload", "bs_kib", "jobs", "runs",
            "fio_mib_s_mean", "fio_mib_s_stdev", "including_sync_mib_s_mean",
            "mean_latency_us", "p99_latency_us", "old_fio_mib_s_mean",
            "change_percent",
        ],
    )


if __name__ == "__main__":
    main()
