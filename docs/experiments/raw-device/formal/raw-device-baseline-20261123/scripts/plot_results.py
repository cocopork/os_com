#!/usr/bin/env python3
"""Plot Linux and Asterinas raw-device bandwidth summaries."""

import csv
from pathlib import Path
import statistics
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


DEVICES = ("nvme", "sata")
SYSTEMS = ("asterinas", "linux")
LABELS = {"asterinas": "Asterinas", "linux": "Linux"}
COLORS = {"asterinas": "#1b9e77", "linux": "#222222"}
WORKLOADS = (
    ("append-write", "Sequential write"),
    ("overwrite-randwrite", "Random write"),
    ("cold-randread", "Random read"),
)


def load_rows(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source))
    for row in rows:
        for field in ("bs_kib", "jobs", "repeat"):
            row[field] = int(row[field])
        row["fio_mib_s"] = float(row["fio_mib_s"])
    return rows


def configure_x_axis(subplot, axis: str, points: list[int]) -> None:
    if axis == "bs":
        subplot.set_xscale("log", base=2)
        labels = [f"{point}K" for point in points]
    else:
        labels = [str(point) for point in points]
    subplot.set_xticks(points, labels=labels)


def build_bandwidth_summary(rows: list[dict], axis: str):
    if axis not in ("bs", "threads"):
        raise ValueError(f"unsupported axis: {axis}")
    x_field = "bs_kib" if axis == "bs" else "jobs"
    figure, axes = plt.subplots(2, 3, figsize=(16, 8.5))
    figure.subplots_adjust(
        left=0.075, right=0.99, bottom=0.08, top=0.90,
        wspace=0.20, hspace=0.26,
    )
    legend_handles = []
    legend_labels = []
    for row_index, device in enumerate(DEVICES):
        for column_index, (workload, title) in enumerate(WORKLOADS):
            subplot = axes[row_index, column_index]
            selected = [
                row for row in rows
                if row["device"] == device
                and row["workload"] == workload
                and row["axis"] == axis
            ]
            points = sorted({row[x_field] for row in selected})
            for system in SYSTEMS:
                system_rows = [row for row in selected if row["system"] == system]
                xs = sorted({row[x_field] for row in system_rows})
                if not xs:
                    continue
                means = [
                    statistics.mean(
                        row["fio_mib_s"] for row in system_rows
                        if row[x_field] == x
                    )
                    for x in xs
                ]
                line, = subplot.plot(
                    xs, means, color=COLORS[system], marker="o",
                    label=LABELS[system],
                )
                subplot.scatter(
                    [row[x_field] for row in system_rows],
                    [row["fio_mib_s"] for row in system_rows],
                    color=COLORS[system], marker="x", alpha=0.55,
                )
                if row_index == 0 and column_index == 0:
                    legend_handles.append(line)
                    legend_labels.append(LABELS[system])
            if row_index == 0:
                subplot.set_title(title)
            if column_index == 0:
                device_label = "NVMe" if device == "nvme" else "SATA"
                subplot.set_ylabel(f"{device_label}\nBandwidth (MiB/s)")
            if row_index == 1:
                subplot.set_xlabel("Block size" if axis == "bs" else "Threads")
            subplot.grid(True, alpha=0.25)
            configure_x_axis(subplot, axis, points)
    figure.legend(
        legend_handles, legend_labels, loc="upper center",
        bbox_to_anchor=(0.5, 0.995), ncol=len(legend_labels),
    )
    return figure, axes


def plot_all(results_csv: Path, output_dir: Path) -> None:
    rows = load_rows(results_csv)
    output_dir.mkdir(parents=True, exist_ok=True)
    for axis, filename in (
        ("bs", "bandwidth-bs-summary.png"),
        ("threads", "bandwidth-thread-summary.png"),
    ):
        figure, _ = build_bandwidth_summary(rows, axis)
        figure.savefig(output_dir / filename, dpi=180)
        plt.close(figure)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} RESULTS.csv OUTPUT_DIR")
    plot_all(Path(sys.argv[1]), Path(sys.argv[2]))


if __name__ == "__main__":
    main()
