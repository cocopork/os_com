#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Plot the validated EXT4 and rNVPCe overwrite sweep."""

import argparse
import csv
from dataclasses import dataclass
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D


MODES = ("ext4", "rnvpce")
WORKLOADS = ("randwrite", "write")
AXES = ("threads", "bs")
COLORS = {"ext4": "#2878B5", "rnvpce": "#D95319"}
LABELS = {"ext4": "EXT4 writeback", "rnvpce": "rNVPCe (4 workers)"}
WORKLOAD_LABELS = {"randwrite": "Random overwrite", "write": "Sequential overwrite"}


@dataclass(frozen=True)
class Result:
    x: int
    fio_mib_s: float
    sync_mib_s: float


def load_matrix(csv_path: Path) -> dict:
    matrix = {
        axis: {
            workload: {mode: [] for mode in MODES}
            for workload in WORKLOADS
        }
        for axis in AXES
    }
    with csv_path.open(newline="") as source:
        for raw in csv.DictReader(source):
            axis = raw["axis"]
            workload = raw["workload"]
            mode = raw["mode"]
            if axis not in AXES or workload not in WORKLOADS or mode not in MODES:
                continue
            x = int(raw["jobs"] if axis == "threads" else raw["bs_kib"])
            matrix[axis][workload][mode].append(
                Result(x, float(raw["fio_mib_s"]), float(raw["including_sync_mib_s"]))
            )
    for axis in AXES:
        expected_x = None
        for workload in WORKLOADS:
            for mode in MODES:
                series = sorted(matrix[axis][workload][mode], key=lambda row: row.x)
                if not series:
                    raise ValueError(f"missing series: {axis}/{workload}/{mode}")
                matrix[axis][workload][mode] = series
                current_x = [row.x for row in series]
                if expected_x is None:
                    expected_x = current_x
                elif current_x != expected_x:
                    raise ValueError(f"incomplete matrix: {axis}/{workload}/{mode}")
    return matrix


def _plot_axis(axis, series, axis_name: str) -> None:
    for mode in MODES:
        rows = series[mode]
        x = [row.x for row in rows]
        axis.plot(
            x,
            [row.fio_mib_s for row in rows],
            color=COLORS[mode],
            marker="o",
            linewidth=2.2,
            markersize=5.5,
        )
    axis.set_axisbelow(True)
    axis.grid(True, which="major", color="#D9D9D9", linewidth=0.8)
    axis.grid(True, which="minor", color="#EEEEEE", linewidth=0.5)
    axis.set_ylabel("Throughput (MiB/s)")
    axis.set_ylim(bottom=0)
    if axis_name == "threads":
        ticks = [row.x for row in series["ext4"]]
        axis.set_xlabel("fio jobs (4 KiB per request)")
        axis.set_xticks(ticks)
    else:
        ticks = [row.x for row in series["ext4"]]
        axis.set_xlabel("Block size (KiB, 1 job)")
        axis.set_xscale("log", base=2)
        axis.set_xticks(ticks, labels=[str(tick) for tick in ticks])
        axis.minorticks_off()


def _repeat_paths(repeat_csv: Path | list[Path] | None) -> list[Path]:
    if repeat_csv is None:
        return []
    if isinstance(repeat_csv, Path):
        return [repeat_csv]
    return repeat_csv


def _overlay_repeats(axes, repeat_csv: Path | list[Path] | None, axis_name: str) -> None:
    paths = [path for path in _repeat_paths(repeat_csv) if path.exists()]
    if not paths:
        return
    repeats = {}
    for path in paths:
        for axis, workloads in load_matrix_partial(path).items():
            for workload, modes in workloads.items():
                for mode, rows in modes.items():
                    repeats.setdefault(axis, {}).setdefault(workload, {}).setdefault(mode, []).extend(rows)
    for plot_axis, workload in zip(axes, WORKLOADS):
        repeat_max = 0.0
        for mode in MODES:
            for row in repeats.get(axis_name, {}).get(workload, {}).get(mode, []):
                repeat_max = max(repeat_max, row.fio_mib_s)
                plot_axis.scatter(
                    row.x,
                    row.fio_mib_s,
                    marker="X",
                    s=80,
                    color=COLORS[mode],
                    edgecolor="white",
                    linewidth=0.8,
                    zorder=5,
                )
        if repeat_max:
            plot_axis.set_ylim(0, max(plot_axis.get_ylim()[1], repeat_max * 1.06))


def load_matrix_partial(csv_path: Path) -> dict:
    matrix = {}
    with csv_path.open(newline="") as source:
        for raw in csv.DictReader(source):
            axis = raw["axis"]
            workload = raw["workload"]
            mode = raw["mode"]
            x = int(raw["jobs"] if axis == "threads" else raw["bs_kib"])
            matrix.setdefault(axis, {}).setdefault(workload, {}).setdefault(mode, []).append(
                Result(x, float(raw["fio_mib_s"]), float(raw["including_sync_mib_s"]))
            )
    return matrix


def _legend_handles(include_repeat: bool) -> list:
    handles = [
        Line2D([0], [0], color=COLORS[mode], linewidth=2.5, label=LABELS[mode])
        for mode in MODES
    ]
    if include_repeat:
        handles.append(
            Line2D(
                [0], [0], color="#555555", marker="X", linewidth=0,
                markersize=8, label="repeat samples (fio)"
            )
        )
    return handles


def render(
    csv_path: Path,
    output_dir: Path,
    repeat_csv: Path | list[Path] | None = None,
) -> list[Path]:
    matrix = load_matrix(csv_path)
    output_dir.mkdir(parents=True, exist_ok=True)
    outputs = []
    for axis_name, filename in (
        ("threads", "ext4-rnvpce-4worker-thread-sweep.png"),
        ("bs", "ext4-rnvpce-4worker-bs-sweep.png"),
    ):
        figure, axes = plt.subplots(1, 2, figsize=(14, 5.8), constrained_layout=True)
        for plot_axis, workload in zip(axes, WORKLOADS):
            _plot_axis(plot_axis, matrix[axis_name][workload], axis_name)
            plot_axis.set_title(WORKLOAD_LABELS[workload], fontsize=13, fontweight="semibold")
        _overlay_repeats(axes, repeat_csv, axis_name)
        figure.suptitle(
            "EXT4 writeback vs rNVPCe (4 workers): "
            + ("thread sweep" if axis_name == "threads" else "block-size sweep"),
            fontsize=16,
            fontweight="bold",
        )
        figure.legend(
            handles=_legend_handles(bool(_repeat_paths(repeat_csv))),
            loc="lower center",
            bbox_to_anchor=(0.5, -0.035),
            ncol=3,
            frameon=False,
        )
        output = output_dir / filename
        figure.savefig(output, dpi=180, bbox_inches="tight", facecolor="white")
        plt.close(figure)
        outputs.append(output)
    return outputs


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results_csv", type=Path)
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--repeat-csv", type=Path, action="append")
    args = parser.parse_args()
    for output in render(args.results_csv, args.output_dir, args.repeat_csv):
        print(output)


if __name__ == "__main__":
    main()
