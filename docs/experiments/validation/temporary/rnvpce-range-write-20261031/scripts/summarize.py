#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Print medians from a validated range-write results.csv."""

import argparse
import csv
import statistics
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    rows = list(csv.DictReader((args.artifacts / "results.csv").open()))
    for jobs in (1, 16):
        selected = [row for row in rows if int(row["jobs"]) == jobs]
        if len(selected) != 3:
            raise SystemExit(f"expected three validated samples for jobs={jobs}")
        fio = statistics.median(float(row["fio_mib_s"]) for row in selected)
        durable = statistics.median(float(row["durable_mib_s"]) for row in selected)
        print(f"jobs={jobs} fio_mib_s={fio:.2f} durable_mib_s={durable:.2f}")


if __name__ == "__main__":
    main()
