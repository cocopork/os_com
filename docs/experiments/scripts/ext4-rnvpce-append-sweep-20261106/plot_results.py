#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Plot the validated EXT4 and rNVPCe first-write append sweep."""

import argparse
import csv
from dataclasses import dataclass
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D


MODES = ("ext4", "rnvpce")
COLORS = {"ext4": "#2878B5", "rnvpce": "#D95319"}
LABELS = {"ext4": "EXT4 writeback", "rnvpce": "rNVPCe (4 workers)"}


@dataclass(frozen=True)
class Result:
    x: int
    fio_mib_s: float
    sync_mib_s: float


def load_results(csv_path: Path) -> dict:
    results = {"threads": {mode: [] for mode in MODES}, "bs": {mode: [] for mode in MODES}}
    with csv_path.open(newline="") as source:
        for raw in csv.DictReader(source):
            axis = raw["axis"]
            mode = raw["mode"]
            if axis not in results or mode not in MODES:
                continue
            x = int(raw["jobs"] if axis == "threads" else raw["bs_kib"])
            results[axis][mode].append(
                Result(x, float(raw["fio_mib_s"]), float(raw["including_sync_mib_s"]))
            )
    for axis in results:
        expected_x = None
        for mode in MODES:
            series = sorted(results[axis][mode], key=lambda row: row.x)
            if not series:
                raise ValueError(f"missing series: {axis}/{mode}")
            results[axis][mode] = series
            current_x = [row.x for row in series]
            if expected_x is None:
                expected_x = current_x
            elif current_x != expected_x:
                raise ValueError(f"incomplete matrix: {axis}/{mode}")
    return results


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


def _repeat_paths(repeat_csv: Path | list[Path] | None) -> list[Path]:
    if repeat_csv is None:
        return []
    if isinstance(repeat_csv, Path):
        return [repeat_csv]
    return repeat_csv


def _overlay_repeats(
    axis,
    repeat_csv: Path | list[Path] | None,
    axis_name: str,
) -> None:
    repeat_max = 0.0
    for path in _repeat_paths(repeat_csv):
        if not path.exists():
            continue
        with path.open(newline="") as source:
            for raw in csv.DictReader(source):
                mode = raw["mode"]
                if raw["axis"] != axis_name or mode not in MODES:
                    continue
                x = int(raw["jobs"] if axis_name == "threads" else raw["bs_kib"])
                throughput = float(raw["fio_mib_s"])
                repeat_max = max(repeat_max, throughput)
                axis.scatter(
                    x,
                    throughput,
                    marker="X",
                    s=80,
                    color=COLORS[mode],
                    edgecolor="white",
                    linewidth=0.8,
                    zorder=5,
                )
    if repeat_max:
        axis.set_ylim(0, max(axis.get_ylim()[1], repeat_max * 1.06))


def render(
    csv_path: Path,
    output_dir: Path,
    repeat_csv: Path | list[Path] | None = None,
) -> list[Path]:
    results = load_results(csv_path)
    output_dir.mkdir(parents=True, exist_ok=True)
    outputs = []
    configurations = (
        (
            "threads",
            "ext4-rnvpce-4worker-append-thread-sweep.png",
            "First-write append: thread sweep",
            "fio jobs (4 KiB per request)",
        ),
        (
            "bs",
            "ext4-rnvpce-4worker-append-bs-sweep.png",
            "First-write append: block-size sweep",
            "Block size (KiB, 1 job)",
        ),
    )
    for axis_name, filename, title, xlabel in configurations:
        figure, axis = plt.subplots(figsize=(10.5, 6.2))
        figure.subplots_adjust(bottom=0.18)
        for mode in MODES:
            rows = results[axis_name][mode]
            x = [row.x for row in rows]
            axis.plot(
                x, [row.fio_mib_s for row in rows], color=COLORS[mode],
                marker="o", linewidth=2.4, markersize=6
            )
        ticks = [row.x for row in results[axis_name]["ext4"]]
        if axis_name == "bs":
            axis.set_xscale("log", base=2)
            axis.set_xticks(ticks, labels=[str(tick) for tick in ticks])
            axis.minorticks_off()
        else:
            axis.set_xticks(ticks)
        axis.set_title(title, fontsize=16, fontweight="bold")
        axis.set_xlabel(xlabel)
        axis.set_ylabel("Throughput (MiB/s)")
        axis.set_ylim(bottom=0)
        axis.grid(True, color="#D9D9D9", linewidth=0.8)
        axis.set_axisbelow(True)
        _overlay_repeats(axis, repeat_csv, axis_name)
        figure.legend(
            handles=_legend_handles(bool(_repeat_paths(repeat_csv))),
            loc="lower center", bbox_to_anchor=(0.5, 0.015),
            ncol=3, frameon=False
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
