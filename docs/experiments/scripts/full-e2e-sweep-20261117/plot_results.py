#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Plots raw repetitions and mean lines for the full end-to-end sweep."""

import csv
from pathlib import Path
import statistics
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


DEVICES = ("nvme", "sata")
MODES = ("ext4", "rnvpce")
LABELS = {"ext4": "EXT4", "rnvpce": "rNVPCe-SU"}
COLORS = {"ext4": "#222222", "rnvpce": "#1b9e77"}
METRICS = (
    ("fio_mib_s", "Bandwidth (MiB/s)", "bandwidth"),
    ("mean_latency_us", "Mean latency (us)", "mean-latency"),
    ("p99_latency_us", "P99 latency (us)", "p99-latency"),
)


def load_rows(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source))
    for row in rows:
        for field in ("bs_kib", "jobs", "repeat"):
            row[field] = int(row[field])
        for field in ("fio_mib_s", "mean_latency_us", "p99_latency_us"):
            row[field] = float(row[field])
    return rows


def plot_metric(rows: list[dict], workload: str, axis: str, metric: str,
                ylabel: str, suffix: str, output: Path) -> None:
    figure, axes = plt.subplots(2, 1, figsize=(8, 8), sharex=True)
    for subplot, device in zip(axes, DEVICES):
        selected = [row for row in rows if row["device"] == device and
                    row["workload"] == workload and row["axis"] == axis]
        for mode in MODES:
            mode_rows = [row for row in selected if row["mode"] == mode]
            x_field = "jobs" if axis == "threads" else "bs_kib"
            xs = sorted({row[x_field] for row in mode_rows})
            means = [statistics.mean(row[metric] for row in mode_rows if row[x_field] == x)
                     for x in xs]
            if not xs:
                continue
            subplot.plot(xs, means, color=COLORS[mode], marker="o", label=LABELS[mode])
            subplot.scatter([row[x_field] for row in mode_rows],
                            [row[metric] for row in mode_rows], marker="x",
                            color=COLORS[mode], alpha=0.55)
        subplot.set_title(device.upper())
        subplot.set_ylabel(ylabel)
        subplot.grid(True, alpha=0.25)
        if axis == "bs":
            subplot.set_xscale("log", base=2)
        subplot.legend()
    axes[-1].set_xlabel("Threads" if axis == "threads" else "Block size (KiB)")
    figure.suptitle(f"{workload}: {axis}")
    figure.tight_layout()
    figure.savefig(output / f"{workload}-{axis}-{suffix}.png", dpi=180)
    plt.close(figure)


def plot_all(results_csv: Path, output: Path) -> None:
    rows = load_rows(results_csv)
    output.mkdir(parents=True, exist_ok=True)
    combinations = sorted({(row["workload"], row["axis"]) for row in rows})
    for workload, axis in combinations:
        for metric, ylabel, suffix in METRICS:
            plot_metric(rows, workload, axis, metric, ylabel, suffix, output)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} RESULTS.csv OUTPUT_DIR")
    plot_all(Path(sys.argv[1]), Path(sys.argv[2]))


if __name__ == "__main__":
    main()
