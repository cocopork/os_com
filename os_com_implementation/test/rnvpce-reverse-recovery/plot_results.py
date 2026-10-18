#!/usr/bin/env python3
"""Plot rNVPCe recovery scaling from the retained summary."""

from __future__ import annotations

import csv
import sys
from pathlib import Path

import matplotlib.pyplot as plt


PHASES = [
    ("descriptor_scan_median_ms", "Descriptor scan"),
    ("runtime_rebuild_median_ms", "Runtime rebuild"),
    ("metadata_discovery_median_ms", "Root discovery"),
    ("pending_repair_median_ms", "Pending repair"),
    ("extent_scan_median_ms", "Extent scan"),
    ("reverse_build_median_ms", "Reverse build"),
    ("allocation_scan_median_ms", "Allocation scan"),
    ("reclaim_median_ms", "Reclaim"),
    ("data_index_restore_median_ms", "Data index restore"),
]


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} RESULT_DIR", file=sys.stderr)
        return 2

    result_dir = Path(sys.argv[1])
    with (result_dir / "summary.csv").open(newline="") as source:
        rows = list(csv.DictReader(source))

    labels = ["1", "1K", "1M"]
    x = range(len(rows))
    fig, axes = plt.subplots(1, 2, figsize=(7.2, 2.8), constrained_layout=True)

    totals = [float(row["total_median_ms"]) / 1000 for row in rows]
    low = [
        (float(row["total_median_ms"]) - float(row["total_min_ms"])) / 1000
        for row in rows
    ]
    high = [
        (float(row["total_max_ms"]) - float(row["total_median_ms"])) / 1000
        for row in rows
    ]
    axes[0].errorbar(x, totals, yerr=[low, high], marker="o", capsize=3, color="#2f6f9f")
    axes[0].set_xticks(list(x), labels)
    axes[0].set_xlabel("Number of files")
    axes[0].set_ylabel("Recovery time (s)")
    axes[0].grid(axis="y", color="#dddddd", linewidth=0.7)

    bottoms = [0.0] * len(rows)
    colors = plt.get_cmap("tab10").colors
    for index, (field, label) in enumerate(PHASES):
        values = [float(row[field]) / 1000 for row in rows]
        axes[1].bar(labels, values, bottom=bottoms, label=label, color=colors[index])
        bottoms = [bottom + value for bottom, value in zip(bottoms, values)]
    axes[1].set_xlabel("Number of files")
    axes[1].set_ylabel("Median phase time (s)")
    axes[1].grid(axis="y", color="#dddddd", linewidth=0.7)

    handles, legend_labels = axes[1].get_legend_handles_labels()
    fig.legend(handles, legend_labels, loc="outside lower center", ncol=3, frameon=False)
    fig.savefig(result_dir / "recovery-scaling.png", dpi=300, bbox_inches="tight")
    fig.savefig(result_dir / "recovery-scaling.pdf", bbox_inches="tight")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
