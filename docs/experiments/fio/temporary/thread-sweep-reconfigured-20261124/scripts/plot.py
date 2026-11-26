#!/usr/bin/env python3
"""Plots the reconfigured append, overwrite, and cold-read thread sweep."""

import csv
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parent.parent
MODES = ("ext4", "rnvpce", "nvlog")
LABELS = {"ext4": "EXT4", "rnvpce": "rNVPCe-SU", "nvlog": "NVLog"}
COLORS = {"ext4": "#222222", "rnvpce": "#1b9e77", "nvlog": "#d95f02"}
THREADS = (1, 4, 8, 12, 16, 20, 24, 28, 32)


def main() -> None:
    with (ROOT / "results.csv").open(newline="") as source:
        rows = list(csv.DictReader(source))
    for row in rows:
        row["jobs"] = int(row["jobs"])
        row["fio_mib_s"] = float(row["fio_mib_s"])

    figure, axes = plt.subplots(2, 3, figsize=(17, 8))
    workloads = (
        ("append-write", "Append write"),
        ("overwrite-randwrite", "Random overwrite"),
        ("cold-randread", "Cold random read"),
    )
    for row_index, device in enumerate(("nvme", "sata")):
        for column_index, (workload, title) in enumerate(workloads):
            axis = axes[row_index, column_index]
            for mode in MODES:
                selected = [
                    row for row in rows
                    if row["device"] == device
                    and row["workload"] == workload
                    and row["mode"] == mode
                ]
                means = [
                    sum(row["fio_mib_s"] for row in selected if row["jobs"] == jobs) / 3
                    for jobs in THREADS
                ]
                axis.plot(THREADS, means, marker="o", color=COLORS[mode], label=LABELS[mode])
                axis.scatter(
                    [row["jobs"] for row in selected],
                    [row["fio_mib_s"] for row in selected],
                    marker="x", color=COLORS[mode], alpha=0.5,
                )
            axis.set_xticks(THREADS, [str(value) for value in THREADS])
            axis.grid(True, alpha=0.25)
            if row_index == 0:
                axis.set_title(title)
            if column_index == 0:
                axis.set_ylabel(f"{device.upper()}\nBandwidth (MiB/s)")
            if row_index == 1:
                axis.set_xlabel("Jobs")
    handles, labels = axes[0, 0].get_legend_handles_labels()
    figure.legend(handles, labels, loc="upper center", ncol=3)
    figure.subplots_adjust(top=0.91, hspace=0.25, wspace=0.18)
    figure.savefig(ROOT / "plots" / "bandwidth-thread-summary.png", dpi=180)


if __name__ == "__main__":
    main()
