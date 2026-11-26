#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import csv
import sys
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np

WORKLOADS = ["fileserver", "varmail", "webserver-10r1w", "webserver-1r1w"]
LABELS = ["fileserver\nR:W=1:2", "varmail", "webserver\nR:W=10:1", "webserver\nR:W=1:1"]


def main():
    root = Path(sys.argv[1])
    rows = list(csv.DictReader((root / "summary.csv").open()))
    for device in ("nvme", "sata"):
        values = {(r["mode"], r["workload"]): float(r["ops_s_median"]) for r in rows if r["device"] == device}
        if any((mode, workload) not in values for mode in ("ext4", "rnvpce") for workload in WORKLOADS):
            continue
        x = np.arange(len(WORKLOADS))
        width = 0.36
        fig, ax = plt.subplots(figsize=(9, 4.8))
        for offset, mode, label, color in [(-width / 2, "ext4", "EXT4 writeback", "#3b6ea8"),
                                            (width / 2, "rnvpce", "rNVPCe-SU + rTIO", "#c4513c")]:
            data = [values[(mode, workload)] for workload in WORKLOADS]
            bars = ax.bar(x + offset, data, width, label=label, color=color)
            ax.bar_label(bars, fmt="%.0f", padding=3, fontsize=8)
        ax.set_xticks(x, LABELS)
        ax.set_ylabel("Filebench throughput (ops/s)")
        ax.set_title(f"{device.upper()} Filebench, 16 threads, median of 3 runs")
        ax.grid(axis="y", alpha=0.25)
        ax.legend()
        fig.tight_layout()
        fig.savefig(root / f"{device}-filebench-ops.png", dpi=180)
        plt.close(fig)


if __name__ == "__main__":
    main()
