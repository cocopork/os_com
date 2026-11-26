#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validate and merge NVLog results into the archived full fio matrix."""

import argparse
import csv
import json
from pathlib import Path

import plot_results
import summarize
import sweep


BASE_MODES = ("ext4", "rnvpce")


def load_csv(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        return list(csv.DictReader(source))


def expected_cases(mode: str) -> set[str]:
    return {
        point._replace(mode=mode).case
        for point in sweep.matrix()
        if point.mode == "ext4"
    }


def validate_cases(rows: list[dict], expected: set[str], label: str) -> None:
    cases = [row["case"] for row in rows]
    duplicates = sorted(case for case in set(cases) if cases.count(case) > 1)
    actual = set(cases)
    if duplicates:
        raise ValueError(f"{label}: duplicate cases: {duplicates[:3]}")
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise ValueError(
            f"{label}: incomplete matrix; missing={missing[:3]} extra={extra[:3]}"
        )


def load_nvlog(root: Path) -> list[dict]:
    rows = []
    for path in sorted(root.glob("*/result.json")):
        rows.append(summarize.normalize_nvlog_result(json.loads(path.read_text())))
    validate_cases(rows, expected_cases("nvlog"), "NVLog")
    return rows


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_results", type=Path)
    parser.add_argument("nvlog_root", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--skip-plots", action="store_true")
    args = parser.parse_args()

    base = [row for row in load_csv(args.base_results) if row["mode"] in BASE_MODES]
    expected_base = expected_cases("ext4") | expected_cases("rnvpce")
    validate_cases(base, expected_base, "EXT4/rNVPCe")
    nvlog = load_nvlog(args.nvlog_root)
    combined = sorted(base + nvlog, key=lambda row: row["case"])

    args.output.mkdir(parents=True, exist_ok=True)
    summarize.write_csv(args.output / "nvlog-results.csv", nvlog)
    summarize.write_csv(args.output / "results.csv", combined)
    aggregates = summarize.aggregate(combined)
    summarize.write_csv(args.output / "aggregate.csv", aggregates)
    if not args.skip_plots:
        plot_results.plot_all(args.output / "results.csv", args.output / "plots")
    print(
        f"integrated {len(combined)}/972 runs, "
        f"aggregated {len(aggregates)}/324 points"
    )


if __name__ == "__main__":
    main()
