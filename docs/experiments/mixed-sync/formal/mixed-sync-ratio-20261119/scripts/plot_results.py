#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Plots mixed-sync throughput and p99 latency with raw repeat markers."""

import argparse
import csv
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

import matrix
import summarize


MODE_ORDER = ("ext2", "ext4", "rnvpce", "nvlog")
MODE_LABELS = {
    "ext2": "EXT2",
    "ext4": "EXT4",
    "rnvpce": "rNVPCe-SU",
    "nvlog": "NVLog",
}
MODE_COLORS = {
    "ext2": "#6b7280",
    "ext4": "#2563a6",
    "rnvpce": "#c2413b",
    "nvlog": "#15803d",
}
METRICS = {
    "operations_per_second": ("Throughput (operations/s)", "operations-per-second"),
    "p99_latency_us": ("p99 operation latency (us)", "p99-latency-us"),
}
SUMMARY_METRICS = {
    "mib_per_second": ("Mixed-sync bandwidth", "MiB/s", "bandwidth"),
    "operations_per_second": ("Mixed-sync IOPS", "operations/s", "iops"),
    "p99_latency_us": ("Mixed-sync p99 latency", "us", "p99-latency"),
}


def build_figure(rows: list[dict], device: str, read_percent: int, metric: str):
    if metric not in METRICS:
        raise ValueError(f"unsupported metric: {metric}")
    figure, axis = plt.subplots(figsize=(8.2, 4.8))
    x_values = list(range(len(matrix.SYNC_PERCENTAGES)))
    raw_label_added = False
    present_modes = [mode for mode in MODE_ORDER if any(row["mode"] == mode for row in rows)]
    center = (len(present_modes) - 1) / 2
    for mode_index, mode in enumerate(present_modes):
        means = []
        for sync_percent in matrix.SYNC_PERCENTAGES:
            samples = [
                float(row[metric])
                for row in rows
                if row["device"] == device
                and row["mode"] == mode
                and int(row["read_percent"]) == read_percent
                and int(row["sync_percent"]) == sync_percent
            ]
            if len(samples) != 3:
                raise ValueError(
                    f"{device}/{mode}/read={read_percent}/sync={sync_percent} has {len(samples)} samples"
                )
            means.append(sum(samples) / len(samples))
            axis.scatter(
                [x_values[matrix.SYNC_PERCENTAGES.index(sync_percent)] + (mode_index - center) * 0.045]
                * len(samples),
                samples,
                marker="x",
                s=28,
                linewidths=1.0,
                color=MODE_COLORS[mode],
                alpha=0.6,
                label="individual rounds" if not raw_label_added else None,
                zorder=3,
            )
            raw_label_added = True
        axis.plot(
            x_values,
            means,
            marker="o",
            linewidth=1.8,
            color=MODE_COLORS[mode],
            label=MODE_LABELS[mode],
        )
    axis.set_xticks(x_values, [f"{value}%" for value in matrix.SYNC_PERCENTAGES])
    axis.set_xlabel("fsync injection ratio")
    axis.set_ylabel(METRICS[metric][0])
    axis.set_title(
        f"{device.upper()} 4 KiB random mixed I/O, read:write={read_percent}:{100-read_percent}"
    )
    axis.grid(axis="y", alpha=0.25)
    axis.legend()
    figure.tight_layout()
    return figure, axis


def build_summary_figure(rows: list[dict], metric: str):
    if metric not in SUMMARY_METRICS:
        raise ValueError(f"unsupported summary metric: {metric}")
    title, unit, _ = SUMMARY_METRICS[metric]
    figure, axes = plt.subplots(
        len(matrix.DEVICES),
        len(matrix.READ_PERCENTAGES),
        figsize=(20, 8.6),
        sharex=True,
        sharey="row",
        squeeze=False,
    )
    x_values = list(range(len(matrix.SYNC_PERCENTAGES)))
    present_modes = [mode for mode in MODE_ORDER if any(row["mode"] == mode for row in rows)]
    center = (len(present_modes) - 1) / 2
    raw_label_added = False
    for device_index, device in enumerate(matrix.DEVICES):
        for read_index, read_percent in enumerate(matrix.READ_PERCENTAGES):
            axis = axes[device_index, read_index]
            for mode_index, mode in enumerate(present_modes):
                means = []
                for sync_index, sync_percent in enumerate(matrix.SYNC_PERCENTAGES):
                    samples = [
                        float(row[metric])
                        for row in rows
                        if row["device"] == device
                        and row["mode"] == mode
                        and int(row["read_percent"]) == read_percent
                        and int(row["sync_percent"]) == sync_percent
                    ]
                    if len(samples) != 3:
                        raise ValueError(
                            f"{device}/{mode}/read={read_percent}/sync={sync_percent} "
                            f"has {len(samples)} samples"
                        )
                    means.append(sum(samples) / len(samples))
                    axis.scatter(
                        [sync_index + (mode_index - center) * 0.045] * len(samples),
                        samples,
                        marker="x",
                        s=17,
                        linewidths=0.8,
                        color=MODE_COLORS[mode],
                        alpha=0.55,
                        label="individual rounds" if not raw_label_added else None,
                        zorder=3,
                    )
                    raw_label_added = True
                axis.plot(
                    x_values,
                    means,
                    marker="o",
                    markersize=4,
                    linewidth=1.45,
                    color=MODE_COLORS[mode],
                    label=MODE_LABELS[mode],
                )
            if device_index == 0:
                axis.set_title(
                    f"R:W = {read_percent // 10}:{(100 - read_percent) // 10}"
                )
            if read_index == 0:
                axis.set_ylabel(f"{device.upper() if device == 'sata' else 'NVMe'}\n{unit}")
            if device_index == len(matrix.DEVICES) - 1:
                axis.set_xlabel("fsync injection ratio")
            axis.set_xticks(
                x_values, [f"{value}%" for value in matrix.SYNC_PERCENTAGES]
            )
            axis.grid(axis="y", alpha=0.25)

    handles, labels = axes[0, 0].get_legend_handles_labels()
    figure.suptitle(title, fontsize=15, y=0.995)
    figure.legend(
        handles,
        labels,
        loc="upper center",
        bbox_to_anchor=(0.5, 0.96),
        ncol=len(labels),
        frameon=True,
    )
    figure.tight_layout(rect=(0, 0, 1, 0.89))
    return figure, axes


def write_summary_plots(rows: list[dict], output: Path) -> None:
    for metric, (_, _, suffix) in SUMMARY_METRICS.items():
        figure, _ = build_summary_figure(rows, metric)
        figure.savefig(output / f"mixed-sync-{suffix}-summary.png", dpi=160)
        plt.close(figure)


def write_plotting_source(path: Path, rows: list[dict]) -> None:
    columns = (
        "case",
        "device",
        "mode",
        "read_percent",
        "write_percent",
        "sync_percent",
        "repeat",
        "operations_per_second",
        "mib_per_second",
        "mean_latency_us",
        "p99_latency_us",
        "actual_read_percent",
        "actual_sync_percent",
        "configured_total_data_cache_mib",
        "cache_control",
    )
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=columns, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)


def generate(rows: list[dict], output: Path) -> None:
    summarize.validate_complete(rows)
    output.mkdir(parents=True, exist_ok=True)
    write_plotting_source(output / "plotting-source.csv", rows)
    write_summary_plots(rows, output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results_csv", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    with args.results_csv.open(newline="") as source:
        rows = list(csv.DictReader(source))
    for row in rows:
        for field in ("read_percent", "write_percent", "sync_percent", "repeat"):
            row[field] = int(row[field])
    generate(rows, args.output)


if __name__ == "__main__":
    main()
