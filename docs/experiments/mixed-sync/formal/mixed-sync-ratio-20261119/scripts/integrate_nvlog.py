#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Integrate a complete NVLog matrix into the archived mixed-sync results."""

import argparse
import csv
import json
from pathlib import Path

import matrix
import plot_results
import summarize


ARCHIVE_MODES = (*matrix.MODES, "nvlog")
INTEGER_FIELDS = (
    "read_percent",
    "write_percent",
    "sync_percent",
    "repeat",
    "threads",
    "file_mib",
    "dataset_mib",
)


def load_base_results(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        rows = list(csv.DictReader(source))
    for row in rows:
        for field in INTEGER_FIELDS:
            row[field] = int(row[field])
    unexpected_modes = {row["mode"] for row in rows} - set(ARCHIVE_MODES)
    if unexpected_modes:
        raise ValueError(f"unsupported modes in base results: {sorted(unexpected_modes)}")
    base_rows = [row for row in rows if row["mode"] in matrix.MODES]
    summarize.validate_complete(base_rows, matrix.MODES)
    return base_rows


def load_nvlog_results(root: Path) -> list[dict]:
    rows = []
    for case in sorted(summarize.expected_cases(("nvlog",))):
        path = root / case / "result.json"
        try:
            payload = json.loads(path.read_text())
        except (FileNotFoundError, json.JSONDecodeError, OSError) as error:
            raise ValueError(f"cannot read NVLog result {path}: {error}") from error
        row = summarize.normalize_nvlog_result(payload)
        if row["case"] != case:
            raise ValueError(f"NVLog result case mismatch in {path}")
        rows.append(row)
    summarize.validate_complete(rows, ("nvlog",))
    return rows


def write_plots(rows: list[dict], output: Path) -> None:
    plot_results.write_plotting_source(output / "plotting-source.csv", rows)
    plots = output / "plots"
    plots.mkdir(parents=True, exist_ok=True)
    plot_results.write_summary_plots(rows, plots)


def integrate(base_csv: Path, nvlog_root: Path, output: Path, *, plots: bool) -> None:
    base_rows = load_base_results(base_csv)
    nvlog_rows = load_nvlog_results(nvlog_root)
    combined = base_rows + nvlog_rows
    summarize.validate_complete(combined, ARCHIVE_MODES)
    aggregated = summarize.aggregate(combined, ARCHIVE_MODES)

    output.mkdir(parents=True, exist_ok=True)
    summarize.write_csv(output / "nvlog-results.csv", nvlog_rows)
    summarize.write_csv(output / "results.csv", combined)
    summarize.write_csv(output / "aggregate.csv", aggregated)
    if plots:
        write_plots(combined, output)
    print(
        f"integrated {len(combined)}/576 runs, "
        f"aggregated {len(aggregated)}/192 points"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_results", type=Path)
    parser.add_argument("nvlog_root", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--skip-plots", action="store_true")
    args = parser.parse_args()
    integrate(
        args.base_results,
        args.nvlog_root,
        args.output,
        plots=not args.skip_plots,
    )


if __name__ == "__main__":
    main()
