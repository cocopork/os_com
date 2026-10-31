#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Summarize the EXT4 buffered-write sweep from retained serial logs."""
import argparse
import csv
import json
import re
from pathlib import Path


def fio_payload(text: str, case: str) -> dict:
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\[[0-9;]*(?=\s)", "", text)
    text = re.sub(r"\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*", "", text)
    start = text.index(f"BUFFER_FIO_START {case}")
    end = text.index(f"BUFFER_FIO_END {case}", start)
    body = text[start:end]
    body = re.sub(r"(?<=\d)m(?=[,\s])", "", body)
    body = re.sub(r'(?m)^(\s*)[0-9;m\[]+(\s+")', r"\1\2", body)
    for match in re.finditer(r"\{", body):
        try:
            value = json.JSONDecoder().raw_decode(body[match.start():])[0]
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "jobs" in value:
            return value
    raise ValueError(f"{case}: fio JSON not found")


def extract(path: Path) -> dict:
    text = path.read_text(errors="replace")
    case_match = re.search(r"BUFFER_FIO_START (ext4-wb-(threads|bs)-([^\s]+))", text)
    if not case_match:
        raise ValueError(f"{path}: start marker not found")
    case, axis, value = case_match.groups()
    payload = fio_payload(text, case)
    if len(payload["jobs"]) != 1:
        raise ValueError(f"{case}: expected one group-reported fio job")
    job = payload["jobs"][0]
    write = job["write"]
    options = job["job options"]
    timing = re.search(
        rf"BUFFER_COMPLETE {re.escape(case)} fio_status=(\d+) start_ns=(\d+) "
        r"fio_end_ns=(\d+) sync_end_ns=(\d+) umount_status=(\d+)",
        text,
    )
    if not timing:
        raise ValueError(f"{case}: completion marker not found")
    fio_status, start_ns, fio_end_ns, sync_end_ns, umount_status = map(int, timing.groups())
    expected = (2048 // int(options["numjobs"])) * int(options["numjobs"]) * 1048576
    if job["error"] or fio_status or umount_status or write["io_bytes"] != expected:
        raise ValueError(f"{case}: incomplete result")
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": case,
        "axis": axis,
        "value": value,
        "rw": options["rw"],
        "bs": options["bs"],
        "jobs": int(options["numjobs"]),
        "io_mib": write["io_bytes"] / 1048576,
        "fio_mib_s": write["bw_bytes"] / 1048576,
        "sync_mib_s": write["io_bytes"] / 1048576 / elapsed,
        "fio_seconds": (fio_end_ns - start_ns) / 1e9,
        "sync_seconds": (sync_end_ns - fio_end_ns) / 1e9,
        "mean_us": write["clat_ns"]["mean"] / 1000,
        "p99_us": write["clat_ns"]["percentile"]["99.000000"] / 1000,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    logs = [
        path
        for path in sorted(args.artifacts.glob("ext4-wb-*.log"))
        if not path.name.endswith("-e2fsck.log")
    ]
    rows = [extract(path) for path in logs]
    if len(rows) != 18:
        raise ValueError(f"expected 18 points, found {len(rows)}")
    with (args.artifacts / "results.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    for row in rows:
        print(
            f"{row['case']}: fio={row['fio_mib_s']:.1f} MiB/s, "
            f"sync={row['sync_mib_s']:.1f} MiB/s"
        )


if __name__ == "__main__":
    main()
