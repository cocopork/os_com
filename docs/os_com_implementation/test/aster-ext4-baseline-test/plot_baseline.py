#!/usr/bin/env python3
"""Plot the Asterinas EXT2-like/EXT4 fsync and orderless thread sweep."""

from __future__ import annotations

import argparse
import csv
import math
from pathlib import Path
from typing import NamedTuple, Sequence

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.axes import Axes
from matplotlib.figure import Figure


HERE = Path(__file__).resolve().parent
DEFAULT_RESULT = HERE / "plots/20260921_180514_aster-ext4-baseline"

DEVICES = ("sata", "nvme")
FILESYSTEMS = ("ext2like", "ext4")
WORKLOADS = ("randwrite-4k", "seqwrite-4k")
MODES = ("fsync", "orderless")
NJOBS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
PROFILES = (
    "ext2like-fsync",
    "ext2like-orderless",
    "ext4-fsync",
    "ext4-orderless",
)

PANEL_TITLES = {
    ("sata", "randwrite-4k"): "(a) SATA random write",
    ("sata", "seqwrite-4k"): "(b) SATA sequential write",
    ("nvme", "randwrite-4k"): "(c) NVMe random write",
    ("nvme", "seqwrite-4k"): "(d) NVMe sequential write",
}
PROFILE_STYLES = {
    "ext2like-fsync": ("EXT2-like fsync", "#176B68", "o", "-"),
    "ext2like-orderless": ("EXT2-like orderless", "#B96845", "s", "--"),
    "ext4-fsync": ("EXT4 fsync", "#3274A1", "^", "-."),
    "ext4-orderless": ("EXT4 orderless", "#7A5A99", "D", ":"),
}
OUTPUT_STEM = "asterinas-ext2like-ext4-sync-orderless"

matplotlib.rcParams.update(
    {
        "font.family": "serif",
        "font.serif": ["Times New Roman", "DejaVu Serif"],
        "font.size": 10,
        "axes.titlesize": 10.5,
        "axes.labelsize": 10,
        "xtick.labelsize": 9,
        "ytick.labelsize": 10,
        "legend.fontsize": 9.5,
        "pdf.fonttype": 42,
        "ps.fonttype": 42,
        "axes.unicode_minus": False,
    }
)


class ResultRow(NamedTuple):
    device: str
    filesystem: str
    workload: str
    mode: str
    profile: str
    njobs: int
    bandwidth_mib_s: float
    valid: bool
    note: str


def _read_csv(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        raise ValueError(f"CSV file not found: {path}")
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source))
    if not rows:
        raise ValueError(f"CSV file is empty: {path}")
    return rows


def load_summary(path: Path) -> list[ResultRow]:
    raw_rows = _read_csv(path)
    required = {
        "device",
        "fs",
        "workload",
        "mode",
        "njobs",
        "busy_bw_mib_s",
        "drain_bw_mib_s",
        "valid",
        "note",
    }
    missing = required - set(raw_rows[0])
    if missing:
        raise ValueError(f"baseline summary is missing columns: {sorted(missing)}")

    rows: list[ResultRow] = []
    actual: set[tuple[str, str, str, str, int]] = set()
    duplicate_count = 0
    for raw in raw_rows:
        device = raw["device"]
        filesystem = raw["fs"]
        workload = raw["workload"]
        mode = raw["mode"]
        njobs = int(raw["njobs"])
        point = (device, filesystem, workload, mode, njobs)
        if point in actual:
            duplicate_count += 1
        actual.add(point)

        valid = raw["valid"].strip().lower() in {"1", "true", "yes"}
        metric_column = "drain_bw_mib_s" if mode == "orderless" else "busy_bw_mib_s"
        bandwidth = float(raw[metric_column]) if valid else math.nan
        if valid and (not math.isfinite(bandwidth) or bandwidth < 0):
            raise ValueError(f"invalid bandwidth for point: {point}")
        rows.append(
            ResultRow(
                device=device,
                filesystem=filesystem,
                workload=workload,
                mode=mode,
                profile=f"{filesystem}-{mode}",
                njobs=njobs,
                bandwidth_mib_s=bandwidth,
                valid=valid,
                note=raw["note"],
            )
        )

    expected = {
        (device, filesystem, workload, mode, njobs)
        for device in DEVICES
        for filesystem in FILESYSTEMS
        for workload in WORKLOADS
        for mode in MODES
        for njobs in NJOBS
    }
    if duplicate_count or actual != expected or len(rows) != len(expected):
        raise ValueError(
            "incomplete baseline matrix: "
            f"{len(expected - actual)} missing, "
            f"{len(actual - expected)} unexpected, "
            f"{duplicate_count} duplicate"
        )
    return rows


def _panel_y_max(values: Sequence[float]) -> float:
    finite_values = [value for value in values if math.isfinite(value)]
    maximum = max(finite_values, default=0.0)
    if maximum <= 0:
        return 1.0
    magnitude = 10 ** math.floor(math.log10(maximum))
    step = magnitude / 2
    return math.ceil(maximum * 1.12 / step) * step


def create_figure(rows: Sequence[ResultRow]) -> tuple[Figure, object]:
    values = {
        (row.device, row.workload, row.profile, row.njobs): row.bandwidth_mib_s
        for row in rows
    }
    figure, axes = plt.subplots(2, 2, figsize=(7.05, 5.55), sharex=True)

    for row_index, device in enumerate(DEVICES):
        for column_index, workload in enumerate(WORKLOADS):
            axis: Axes = axes[row_index, column_index]
            panel_values: list[float] = []
            for profile in PROFILES:
                label, color, marker, linestyle = PROFILE_STYLES[profile]
                bandwidths = [
                    values[device, workload, profile, njobs] for njobs in NJOBS
                ]
                panel_values.extend(bandwidths)
                axis.plot(
                    NJOBS,
                    bandwidths,
                    label=label,
                    color=color,
                    marker=marker,
                    linestyle=linestyle,
                    linewidth=1.5,
                    markersize=4.5,
                    markerfacecolor="white",
                    markeredgewidth=1.0,
                )

            panel_limit = max(value for value in panel_values if math.isfinite(value))
            axis.set_ylim(0, _panel_y_max(panel_values))
            axis.axhline(
                panel_limit,
                color="#C62828",
                linestyle="--",
                linewidth=1.2,
                zorder=1,
            )
            axis.annotate(
                f"Device limit: {panel_limit:,.0f} MiB/s",
                xy=(NJOBS[-1], panel_limit),
                xytext=(-2, 3),
                textcoords="offset points",
                ha="right",
                va="bottom",
                color="#C62828",
                fontsize=8.5,
            )
            axis.set_xticks(NJOBS)
            axis.tick_params(axis="x", labelbottom=True)
            axis.grid(axis="y", color="#D6DADF", linestyle=":", linewidth=0.7)
            axis.set_axisbelow(True)
            axis.spines["top"].set_visible(False)
            axis.spines["right"].set_visible(False)
            if column_index == 0:
                axis.set_ylabel("Throughput (MiB/s)")
            axis.set_xlabel("fio jobs", labelpad=2)
            axis.text(
                0.5,
                -0.32,
                PANEL_TITLES[device, workload],
                transform=axis.transAxes,
                ha="center",
                va="top",
                fontsize=10.5,
            )

    handles, labels = axes[0, 0].get_legend_handles_labels()
    figure.legend(
        handles,
        labels,
        loc="upper center",
        bbox_to_anchor=(0.5, 0.985),
        ncol=4,
        frameon=False,
        columnspacing=0.65,
        handlelength=1.7,
        handletextpad=0.35,
    )
    figure.subplots_adjust(
        left=0.105,
        right=0.985,
        bottom=0.16,
        top=0.88,
        wspace=0.25,
        hspace=0.62,
    )
    return figure, axes


def default_output_dir(result_dir: Path) -> Path:
    return HERE / "plots" / result_dir.name


def plot_result(result_dir: Path, output_dir: Path | None = None) -> list[Path]:
    rows = load_summary(result_dir / "summary.csv")
    destination = output_dir or default_output_dir(result_dir)
    destination.mkdir(parents=True, exist_ok=True)
    figure, _ = create_figure(rows)
    outputs = [
        destination / f"{OUTPUT_STEM}.pdf",
        destination / f"{OUTPUT_STEM}.png",
    ]
    figure.savefig(outputs[0], bbox_inches="tight")
    figure.savefig(outputs[1], dpi=300, bbox_inches="tight")
    plt.close(figure)
    for output in outputs:
        print(f"saved {output}")
    return outputs


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result-dir", type=Path, default=DEFAULT_RESULT)
    parser.add_argument("--output-dir", type=Path)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    plot_result(args.result_dir, args.output_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
