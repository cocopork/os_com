#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Aggregate overwrite anomaly repeats without hiding raw samples."""

import argparse
import csv
from collections import defaultdict
from pathlib import Path


def read_rows(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        return list(csv.DictReader(source))


def median_samples(rows: list[dict], expected_rounds: int) -> dict[str, dict]:
    grouped = defaultdict(list)
    for row in rows:
        grouped[row["case"]].append(row)
    selected = {}
    for case, samples in grouped.items():
        rounds = {sample["round"] for sample in samples}
        if len(samples) != expected_rounds or len(rounds) != expected_rounds:
            raise ValueError(f"{case}: expected {expected_rounds} repeats")
        ordered = sorted(samples, key=lambda sample: float(sample["fio_mib_s"]))
        median = dict(ordered[expected_rounds // 2])
        median["selected_round"] = median["round"]
        median["sample_count"] = str(expected_rounds)
        median["statistic"] = "median_fio_sample"
        selected[case] = median
    return selected


def merge_selected(baseline: list[dict], selected: dict[str, dict]) -> list[dict]:
    corrected = []
    for row in baseline:
        if row["case"] in selected:
            replacement = {
                key: value for key, value in selected[row["case"]].items() if key != "round"
            }
            corrected.append(replacement)
        else:
            item = dict(row)
            item.setdefault("sample_count", "1")
            item.setdefault("selected_round", "original")
            item.setdefault("statistic", "single_sample")
            corrected.append(item)
    return corrected


def aggregate(
    baseline_csv: Path,
    repeats: list[tuple[str, Path]],
    raw_output: Path,
    corrected_output: Path,
) -> tuple[int, int]:
    raw_rows = []
    for round_name, path in repeats:
        for row in read_rows(path):
            item = dict(row)
            item["round"] = round_name
            raw_rows.append(item)
    selected = median_samples(raw_rows, expected_rounds=len(repeats))
    baseline = read_rows(baseline_csv)
    baseline_cases = {row["case"] for row in baseline}
    if not set(selected) <= baseline_cases:
        raise ValueError("repeat set contains a case outside the baseline matrix")

    raw_fields = list(raw_rows[0])
    raw_output.parent.mkdir(parents=True, exist_ok=True)
    with raw_output.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=raw_fields)
        writer.writeheader()
        writer.writerows(raw_rows)

    corrected = merge_selected(baseline, selected)
    corrected_fields = list(
        dict.fromkeys(list(baseline[0]) + ["sample_count", "selected_round", "statistic"])
    )
    with corrected_output.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=corrected_fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(corrected)
    return len(raw_rows), len(corrected)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline_csv", type=Path)
    parser.add_argument("raw_output", type=Path)
    parser.add_argument("corrected_output", type=Path)
    parser.add_argument(
        "--repeat",
        action="append",
        required=True,
        metavar="ROUND:CSV",
        help="repeat round name and results.csv path",
    )
    args = parser.parse_args()
    repeats = []
    for value in args.repeat:
        round_name, separator, path = value.partition(":")
        if not separator or not round_name or not path:
            parser.error(f"invalid --repeat value: {value}")
        repeats.append((round_name, Path(path)))
    raw_count, corrected_count = aggregate(
        args.baseline_csv, repeats, args.raw_output, args.corrected_output
    )
    print(f"wrote {raw_count} raw repeats and {corrected_count} corrected matrix rows")


if __name__ == "__main__":
    main()
