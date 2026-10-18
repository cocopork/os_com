#!/usr/bin/env python3
"""Plot the Asterinas EXT2/EXT4 fsync and orderless motivation matrix."""

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
DEFAULT_RESULT = (
    HERE / "plots/motivation-plot"
)

DEVICES = ("sata", "nvme")
WORKLOADS = ("randwrite-4k", "seqwrite-4k")
PROFILES = (
    "ext2like",
    "ext4",
    "ext2like_sync",
    "ext4_sync",
    "ext4-rNVPCe",
)
PROFILE_MODES = {
    "ext2like": "fsync",
    "ext4": "fsync",
    "ext2like_sync": "orderless",
    "ext4_sync": "orderless",
    "ext4-rNVPCe": "orderless",
}
NJOBS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
PANEL_TITLES = {
    ("sata", "randwrite-4k"): "(a) SATA random write",
    ("sata", "seqwrite-4k"): "(b) SATA sequential write",
    ("nvme", "randwrite-4k"): "(c) NVMe random write",
    ("nvme", "seqwrite-4k"): "(d) NVMe sequential write",
}
PROFILE_STYLES = {
    "ext2like": ("EXT2 fsync", "#176B68", "o", "-"),
    "ext4": ("EXT4 fsync", "#B96845", "s", "--"),
    "ext2like_sync": ("EXT2-orderless", "#3274A1", "^", "-."),
    "ext4_sync": ("EXT4-orderless", "#7A5A99", "D", ":"),
    "ext4-rNVPCe": ("rNVPCe", "#4D8267", "v", (0, (3, 1, 1, 1))),
}
IDEAL_COLOR = "#C43C39"
OUTPUT_STEM = "aster-ext4-motivation"

matplotlib.rcParams.update(
    {
        "font.family": "serif",
        "font.serif": ["Times New Roman", "DejaVu Serif"],
        "font.size": 10,
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
    profile: str
    workload: str
    mode: str
    njobs: int
    bandwidth_mib_s: float
    valid: bool
    note: str


def _read_csv(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        raise ValueError(f"summary.csv not found: {path}")
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source))
    if not rows:
        raise ValueError(f"summary.csv is empty: {path}")
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
        "valid",
        "note",
    }
    missing = required - set(raw_rows[0])
    if missing:
        raise ValueError(f"summary.csv is missing columns: {sorted(missing)}")

    rows: list[ResultRow] = []
    actual: set[tuple[str, str, str, str, int]] = set()
    duplicate_count = 0
    for raw in raw_rows:
        point = (
            raw["device"],
            raw["fs"],
            raw["workload"],
            raw["mode"],
            int(raw["njobs"]),
        )
        if point in actual:
            duplicate_count += 1
        actual.add(point)
        valid = raw["valid"].strip().lower() in {"1", "true", "yes"}
        bandwidth = float(raw["busy_bw_mib_s"]) if valid else math.nan
        if valid and (not math.isfinite(bandwidth) or bandwidth <= 0):
            raise ValueError(f"invalid busy bandwidth for point: {point}")
        rows.append(
            ResultRow(
                device=raw["device"],
                profile=raw["fs"],
                workload=raw["workload"],
                mode=raw["mode"],
                njobs=int(raw["njobs"]),
                bandwidth_mib_s=bandwidth,
                valid=valid,
                note=raw["note"],
            )
        )

    expected = {
        (device, profile, workload, PROFILE_MODES[profile], njobs)
        for device in DEVICES
        for profile in PROFILES
        for workload in WORKLOADS
        for njobs in NJOBS
    }
    if duplicate_count or actual != expected or len(rows) != len(expected):
        raise ValueError(
            "incomplete writeback matrix: "
            f"{len(expected - actual)} missing, "
            f"{len(actual - expected)} unexpected, "
            f"{duplicate_count} duplicate"
        )
    return rows


def _linear_y_max(values: Sequence[float]) -> float:
    maximum = max(value for value in values if math.isfinite(value))
    magnitude = 10 ** math.floor(math.log10(maximum))
    step = magnitude / 2
    return math.ceil(maximum * 1.14 / step) * step


def create_figure(
    rows: Sequence[ResultRow], *, log_scale: bool
) -> tuple[Figure, object]:
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
                    linewidth=1.45,
                    markersize=4.2,
                    markerfacecolor="white",
                    markeredgewidth=0.95,
                )

            finite_values = [value for value in panel_values if math.isfinite(value)]
            panel_limit = max(finite_values)
            if log_scale:
                axis.set_yscale("log")
                axis.set_ylim(min(finite_values) * 0.72, panel_limit * 1.45)
            else:
                axis.set_ylim(0, _linear_y_max(finite_values))
            axis.axhline(
                panel_limit,
                label="Ideal",
                color=IDEAL_COLOR,
                linestyle="--",
                linewidth=1.25,
                zorder=1,
            )
            axis.set_xticks(NJOBS)
            axis.tick_params(axis="x", labelbottom=True)
            axis.grid(
                axis="y",
                which="major",
                color="#D6DADF",
                linestyle=":",
                linewidth=0.7,
            )
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
        handles[:6],
        labels[:6],
        loc="upper center",
        bbox_to_anchor=(0.5, 0.985),
        ncol=6,
        frameon=False,
        columnspacing=0.48,
        handlelength=1.45,
        handletextpad=0.3,
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


def _output_suffix(name_suffix: str) -> str:
    suffix = name_suffix.strip("-")
    if suffix and not all(char.isalnum() or char in "_-" for char in suffix):
        raise ValueError("name suffix may contain only letters, digits, '_' and '-'")
    return f"-{suffix}" if suffix else ""


def plot_result(
    result_dir: Path,
    output_dir: Path | None = None,
    *,
    summary_path: Path | None = None,
    name_suffix: str = "",
) -> list[Path]:
    rows = load_summary(summary_path or result_dir / "summary.csv")
    destination = output_dir or result_dir
    destination.mkdir(parents=True, exist_ok=True)
    output_suffix = _output_suffix(name_suffix)
    outputs: list[Path] = []
    for scale, log_scale in (("linear", False), ("log", True)):
        figure, _ = create_figure(rows, log_scale=log_scale)
        scale_outputs = [
            destination / f"{OUTPUT_STEM}-{scale}{output_suffix}.pdf",
            destination / f"{OUTPUT_STEM}-{scale}{output_suffix}.png",
        ]
        figure.savefig(scale_outputs[0], bbox_inches="tight")
        figure.savefig(scale_outputs[1], dpi=300, bbox_inches="tight")
        plt.close(figure)
        outputs.extend(scale_outputs)
    for output in outputs:
        print(f"saved {output}")
    return outputs


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result-dir", type=Path, default=DEFAULT_RESULT)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--summary", type=Path)
    parser.add_argument("--name-suffix", default="")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    plot_result(
        args.result_dir,
        args.output_dir,
        summary_path=args.summary,
        name_suffix=args.name_suffix,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
