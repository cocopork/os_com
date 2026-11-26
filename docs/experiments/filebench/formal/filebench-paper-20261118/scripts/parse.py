#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import argparse
import csv
import json
import re
from pathlib import Path

SUMMARY = re.compile(
    r"IO Summary:\s+(?P<ops>\d+) ops\s+(?P<ops_s>[0-9.]+) ops/s.*?"
    r"(?P<mb_s>[0-9.]+)mb/s\s+(?P<latency>[0-9.]+)ms/(?:op|latency)",
    re.I,
)


def parse_log(path: Path):
    text = path.read_text(errors="replace").replace("\r", "")
    match = list(SUMMARY.finditer(text))
    complete = re.search(r"FILEBENCH_COMPLETE .*?status=(\d+)", text)
    cleanup = re.search(r"FILEBENCH_CLEANUP .*?sync_status=(\d+) umount_status=(\d+)", text)
    if not match or not complete or not cleanup:
        raise ValueError(f"missing Filebench result markers in {path}")
    result = match[-1]
    status = int(complete.group(1))
    sync_status, umount_status = map(int, cleanup.groups())
    if status or sync_status or umount_status:
        raise ValueError(
            f"failed guest run in {path}: filebench={status} sync={sync_status} umount={umount_status}"
        )
    return {
        "operations": int(result.group("ops")),
        "ops_s": float(result.group("ops_s")),
        "mb_s": float(result.group("mb_s")),
        "latency_ms": float(result.group("latency")),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("--case")
    args = parser.parse_args()
    rows = []
    points = [args.root / args.case] if args.case else sorted(args.root.glob("*-r[0-9]"))
    for point in points:
        if not point.is_dir():
            continue
        config = json.loads((point / "case.json").read_text())
        row = {**config, **parse_log(point / "qemu.log")}
        (point / "result.json").write_text(json.dumps(row, indent=2) + "\n")
        rows.append(row)
    if not args.case:
        columns = [
            "case", "device", "mode", "workload", "repeat", "threads", "nfiles",
            "filesize_kib", "dataset_mib", "dram_cache_mib", "nv_data_cache_mib",
            "operations", "ops_s", "mb_s", "latency_ms",
        ]
        with (args.root / "raw-results.csv").open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=columns)
            writer.writeheader()
            writer.writerows(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
