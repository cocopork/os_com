#!/usr/bin/env python3
"""Plot instrumented EXT4 and rNVPCe crash-recovery latency."""

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
from matplotlib.patches import Patch


HERE = Path(__file__).resolve().parent
PROFILES = ("ext4-orderless", "AchillesFS")
DATA_SIZES = (32, 256, 2048)
DATA_LABELS = {32: "4GB", 256: "32GB", 2048: "128GB"}
PROFILE_STYLES = {
    "ext4-orderless": ("EXT4", "#176B68", ""),
    "AchillesFS": ("AchillesFS", "#B96845", "///"),
}
PHASES = (
    ("Super/options", "super_options", "#4C78A8"),
    ("Block groups", "block_groups", "#72B7B2"),
    ("Journal/bitmap scan", "journal", "#F2CF5B"),
    ("Workers/other", "workers", "#B279A2"),
)
OUTPUT_STEM = "crash-recovery-performance"

matplotlib.rcParams.update(
    {
        "font.family": "serif",
        "font.serif": ["Times New Roman", "DejaVu Serif"],
        "font.size": 10,
        "axes.labelsize": 10,
        "xtick.labelsize": 10,
        "ytick.labelsize": 10,
        "legend.fontsize": 10,
        "pdf.fonttype": 42,
        "ps.fonttype": 42,
        "axes.unicode_minus": False,
        "hatch.linewidth": 0.65,
    }
)


class RecoveryRow(NamedTuple):
    profile: str
    data_mb: int
    total_ms: float
    super_options_ms: float
    block_groups_ms: float
    journal_ms: float
    workers_ms: float


def load_summary(path: Path) -> list[RecoveryRow]:
    required = {
        "profile",
        "data_mb",
        "repeats",
        "valid_repeats",
        "mount_total_median_us",
        "mount_super_options_median_us",
        "mount_block_groups_median_us",
        "mount_construct_median_us",
        "mount_journal_median_us",
        "mount_writeback_worker_median_us",
        "mount_rnvpc_worker_median_us",
    }
    with path.open(newline="", encoding="utf-8") as source:
        raw_rows = list(csv.DictReader(source))
    if not raw_rows:
        raise ValueError(f"summary.csv is empty: {path}")
    missing = required - set(raw_rows[0])
    if missing:
        raise ValueError(f"summary.csv is missing columns: {sorted(missing)}")

    rows: list[RecoveryRow] = []
    actual: set[tuple[str, int]] = set()
    for raw in raw_rows:
        profile = raw["profile"]
        data_mb = int(raw["data_mb"])
        point = (profile, data_mb)
        if point in actual:
            raise ValueError(f"duplicate recovery point: {point}")
        actual.add(point)
        if int(raw["repeats"]) != 5 or int(raw["valid_repeats"]) != 5:
            raise ValueError(f"recovery point is not 5/5 valid: {point}")

        micros = lambda name: float(raw[name]) / 1_000.0
        workers_ms = sum(
            micros(name)
            for name in (
                "mount_construct_median_us",
                "mount_writeback_worker_median_us",
                "mount_rnvpc_worker_median_us",
            )
        )
        row = RecoveryRow(
            profile=profile,
            data_mb=data_mb,
            total_ms=micros("mount_total_median_us"),
            super_options_ms=micros("mount_super_options_median_us"),
            block_groups_ms=micros("mount_block_groups_median_us"),
            journal_ms=micros("mount_journal_median_us"),
            workers_ms=workers_ms,
        )
        if not all(math.isfinite(value) and value >= 0 for value in row[2:]):
            raise ValueError(f"invalid latency for recovery point: {point}")
        rows.append(row)

    expected = {(profile, size) for profile in PROFILES for size in DATA_SIZES}
    if actual != expected:
        raise ValueError(
            f"incomplete recovery matrix: missing={sorted(expected - actual)}, "
            f"unexpected={sorted(actual - expected)}"
        )
    return rows


def _style_axis(axis: Axes) -> None:
    axis.grid(axis="y", color="#D6DADF", linestyle=":", linewidth=0.7)
    axis.set_axisbelow(True)
    axis.spines["top"].set_visible(False)
    axis.spines["right"].set_visible(False)


def create_figure(rows: Sequence[RecoveryRow]) -> tuple[Figure, Sequence[Axes]]:
    values = {(row.profile, row.data_mb): row for row in rows}
    figure, axes = plt.subplots(1, 2, figsize=(7.05, 3.05))
    width = 0.34
    x_positions = list(range(len(DATA_SIZES)))

    total_axis = axes[0]
    for profile_index, profile in enumerate(PROFILES):
        label, color, hatch = PROFILE_STYLES[profile]
        offset = (profile_index - 0.5) * width
        heights = [values[profile, size].total_ms for size in DATA_SIZES]
        bars = total_axis.bar(
            [x + offset for x in x_positions],
            heights,
            width=width,
            label=label,
            color=color,
            edgecolor="#333333",
            linewidth=0.65,
            hatch=hatch,
        )
        total_axis.bar_label(bars, fmt="%.1f", padding=2, fontsize=10)
    total_axis.set_xticks(x_positions, [DATA_LABELS[size] for size in DATA_SIZES])
    total_axis.set_ylabel("Recovery latency (ms)")
    total_axis.set_ylim(0, 58)
    total_axis.legend(loc="upper left", frameon=False, ncol=1)
    total_axis.text(
        0.5,
        -0.22,
        "(a) Crash Recovery Latency",
        transform=total_axis.transAxes,
        ha="center",
        va="top",
        fontsize=10.5,
    )
    _style_axis(total_axis)

    phase_axis = axes[1]
    for profile_index, profile in enumerate(PROFILES):
        _, _, hatch = PROFILE_STYLES[profile]
        offset = (profile_index - 0.5) * width
        positions = [x + offset for x in x_positions]
        bottoms = [0.0] * len(DATA_SIZES)
        for _, field, color in PHASES:
            heights = [getattr(values[profile, size], f"{field}_ms") for size in DATA_SIZES]
            phase_axis.bar(
                positions,
                heights,
                width=width,
                bottom=bottoms,
                color=color,
                edgecolor="#333333",
                linewidth=0.55,
                hatch=hatch,
            )
            bottoms = [bottom + height for bottom, height in zip(bottoms, heights)]
    phase_axis.set_xticks(x_positions, [DATA_LABELS[size] for size in DATA_SIZES])
    phase_axis.set_ylabel("Recovery latency (ms)")
    phase_axis.set_ylim(0, 53)
    phase_axis.legend(
        handles=[Patch(facecolor=color, edgecolor="#333333", label=label) for label, _, color in PHASES],
        loc="upper left",
        frameon=False,
        ncol=2,
        columnspacing=0.8,
        handlelength=1.2,
        handletextpad=0.4,
    )
    phase_axis.text(
        0.5,
        -0.22,
        "(b) Crash Recovery Breakdown",
        transform=phase_axis.transAxes,
        ha="center",
        va="top",
        fontsize=10.5,
    )
    _style_axis(phase_axis)

    figure.subplots_adjust(left=0.10, right=0.985, bottom=0.24, top=0.98, wspace=0.30)
    return figure, axes


def plot_result(summary_path: Path, output_dir: Path) -> list[Path]:
    rows = load_summary(summary_path)
    output_dir.mkdir(parents=True, exist_ok=True)
    figure, _ = create_figure(rows)
    outputs = [
        output_dir / f"{OUTPUT_STEM}.pdf",
        output_dir / f"{OUTPUT_STEM}.png",
    ]
    figure.savefig(outputs[0], bbox_inches="tight")
    figure.savefig(outputs[1], dpi=300, bbox_inches="tight")
    plt.close(figure)
    return outputs


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", type=Path, default=HERE / "summary.csv")
    parser.add_argument("--output-dir", type=Path, default=HERE)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    for output in plot_result(args.summary, args.output_dir):
        print(f"saved {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
