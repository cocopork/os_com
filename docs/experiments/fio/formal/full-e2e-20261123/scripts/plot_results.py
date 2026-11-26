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
MODES = ("ext4", "rnvpce", "nvlog")
LABELS = {"ext4": "EXT4", "rnvpce": "rNVPCe-SU", "nvlog": "NVLog"}
COLORS = {"ext4": "#222222", "rnvpce": "#1b9e77", "nvlog": "#d95f02"}
SUMMARY_WORKLOADS = (
    ("append-write", "Append write"),
    ("overwrite-randwrite", "Overwrite"),
    ("cold-randread", "Random read"),
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


def configure_x_axis(subplot, axis: str, test_points: list[int]) -> None:
    if axis == "bs":
        subplot.set_xscale("log", base=2)
        labels = [f"{point}K" for point in test_points]
    else:
        labels = [str(point) for point in test_points]
    subplot.set_xticks(test_points, labels=labels)


def present_modes(rows: list[dict]) -> tuple[str, ...]:
    available = {row["mode"] for row in rows}
    return tuple(mode for mode in MODES if mode in available)


def build_bandwidth_summary(rows: list[dict], axis: str):
    if axis not in ("bs", "threads"):
        raise ValueError(f"unsupported summary axis: {axis}")
    x_field = "bs_kib" if axis == "bs" else "jobs"
    x_label = "Block size" if axis == "bs" else "Threads"
    figure, axes = plt.subplots(2, 3, figsize=(16, 8.5))
    figure.subplots_adjust(
        left=0.075, right=0.99, bottom=0.08, top=0.90,
        wspace=0.20, hspace=0.26,
    )
    legend_handles = []
    legend_labels = []
    for row_index, device in enumerate(DEVICES):
        for column_index, (workload, title) in enumerate(SUMMARY_WORKLOADS):
            subplot = axes[row_index, column_index]
            selected = [
                row for row in rows
                if row["device"] == device
                and row["workload"] == workload
                and row["axis"] == axis
            ]
            test_points = sorted({row[x_field] for row in selected})
            for mode in present_modes(selected):
                mode_rows = [row for row in selected if row["mode"] == mode]
                xs = sorted({row[x_field] for row in mode_rows})
                means = [
                    statistics.mean(
                        row["fio_mib_s"] for row in mode_rows if row[x_field] == x
                    )
                    for x in xs
                ]
                if not xs:
                    continue
                line, = subplot.plot(
                    xs, means, color=COLORS[mode], marker="o", label=LABELS[mode]
                )
                subplot.scatter(
                    [row[x_field] for row in mode_rows],
                    [row["fio_mib_s"] for row in mode_rows],
                    marker="x", color=COLORS[mode], alpha=0.55,
                )
                if row_index == 0 and column_index == 0:
                    legend_handles.append(line)
                    legend_labels.append(LABELS[mode])
            if row_index == 0:
                subplot.set_title(title)
            if column_index == 0:
                device_label = "NVMe" if device == "nvme" else "SATA"
                subplot.set_ylabel(f"{device_label}\nBandwidth (MiB/s)")
            if row_index == len(DEVICES) - 1:
                subplot.set_xlabel(x_label)
            subplot.grid(True, alpha=0.25)
            configure_x_axis(subplot, axis, test_points)
    if legend_handles:
        figure.legend(
            legend_handles, legend_labels, loc="upper center",
            bbox_to_anchor=(0.5, 0.995), ncol=len(legend_labels),
        )
    return figure, axes


def build_bs_bandwidth_summary(rows: list[dict]):
    return build_bandwidth_summary(rows, "bs")


def build_thread_bandwidth_summary(rows: list[dict]):
    return build_bandwidth_summary(rows, "threads")


def plot_all(results_csv: Path, output: Path) -> None:
    rows = load_rows(results_csv)
    output.mkdir(parents=True, exist_ok=True)
    for old_plot in output.glob("*.png"):
        old_plot.unlink()
    summary_workloads = {workload for workload, _ in SUMMARY_WORKLOADS}
    for axis, builder, filename in (
        ("bs", build_bs_bandwidth_summary, "bandwidth-bs-summary.png"),
        ("threads", build_thread_bandwidth_summary, "bandwidth-thread-summary.png"),
    ):
        available_workloads = {
            row["workload"] for row in rows if row["axis"] == axis
        }
        if summary_workloads <= available_workloads:
            summary, _ = builder(rows)
            summary.savefig(output / filename, dpi=180)
            plt.close(summary)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} RESULTS.csv OUTPUT_DIR")
    plot_all(Path(sys.argv[1]), Path(sys.argv[2]))


if __name__ == "__main__":
    main()
