#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validate and summarize the scalable NVPage pool overwrite experiment."""

import argparse
import csv
import json
import re
from pathlib import Path


MIB = 1024 * 1024
CASE_RE = re.compile(r"nvpage-pool-r([123])-write-128k-overwrite-j(1|16)$")


def clean_serial(text: str) -> str:
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\[[0-9;]*(?=\s)", "", text)
    return re.sub(r"\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*", "", text)


def fio_payload(body: str) -> dict:
    body = re.sub(r"(?<=\d)m(?=[,\s])", "", body)
    body = re.sub(r'(?m)^(\s*)[0-9;m\[]+(\s+")', r"\1\2", body)
    for match in re.finditer(r"\{", body):
        try:
            value = json.JSONDecoder().raw_decode(body[match.start() :])[0]
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "jobs" in value:
            return value
    raise ValueError("fio JSON not found")


def parse_case(artifacts: Path, case: str) -> dict:
    match = CASE_RE.fullmatch(case)
    if not match:
        raise ValueError(f"unknown case: {case}")
    round_number, jobs = map(int, match.groups())
    expected_bytes = (2048 // jobs) * jobs * MIB
    log = artifacts / f"{case}.log"
    text = clean_serial(log.read_text(errors="replace"))
    config = (
        f"OVERWRITE_CONFIG case={case} prepare=1 bs=128k jobs={jobs} "
        f"total_mib=2048 expected_bytes={expected_bytes}"
    )
    if config not in text:
        raise ValueError(f"{case}: configuration marker missing")
    if f"OVERWRITE_PREPARE_COMPLETE {case} bytes={expected_bytes} status=0" not in text:
        raise ValueError(f"{case}: preparation marker missing")
    start_marker = f"OVERWRITE_FIO_START {case}"
    end_marker = f"OVERWRITE_FIO_END {case}"
    start = text.index(start_marker) + len(start_marker)
    end = text.index(end_marker, start)
    payload = fio_payload(text[start:end])
    job = payload["jobs"][0]
    options = job["job options"]
    expected_options = {
        "rw": "write",
        "bs": "128k",
        "numjobs": str(jobs),
        "direct": "0",
        "ioengine": "psync",
    }
    for key, expected in expected_options.items():
        if str(options.get(key)) != expected:
            raise ValueError(f"{case}: fio option {key} is not {expected}")
    completion = re.search(
        rf"OVERWRITE_COMPLETE {re.escape(case)} fio_status=(\d+) start_ns=(\d+) "
        r"fio_end_ns=(\d+) sync_end_ns=(\d+) umount_status=(\d+)",
        text,
    )
    if not completion:
        raise ValueError(f"{case}: completion marker missing")
    fio_status, start_ns, fio_end_ns, sync_end_ns, umount_status = map(
        int, completion.groups()
    )
    write = job["write"]
    if int(job.get("error", -1)) or fio_status or umount_status:
        raise ValueError(f"{case}: fio or unmount failed")
    if int(write.get("io_bytes", -1)) != expected_bytes:
        raise ValueError(f"{case}: incomplete byte count")
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{case}: invalid timing order")
    check = (artifacts / f"{case}-e2fsck.log").read_text(errors="replace")
    if "Pass 5: Checking group summary information" not in check:
        raise ValueError(f"{case}: e2fsck incomplete")
    if not re.search(r"OVERWRITE_E2FSCK_STATUS 0(?:\s|$)", check):
        raise ValueError(f"{case}: e2fsck failed")
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": case,
        "round": round_number,
        "jobs": jobs,
        "fio_mib_s": int(write["bw_bytes"]) / MIB,
        "durable_mib_s": expected_bytes / MIB / elapsed,
        "fio_seconds": (fio_end_ns - start_ns) / 1e9,
        "sync_seconds": (sync_end_ns - fio_end_ns) / 1e9,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    validate = subparsers.add_parser("validate")
    validate.add_argument("artifacts", type=Path)
    validate.add_argument("case")
    summarize = subparsers.add_parser("summarize")
    summarize.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    if args.command == "validate":
        parse_case(args.artifacts, args.case)
        print(f"{args.case}: complete")
        return
    cases = [
        f"nvpage-pool-r{round_number}-write-128k-overwrite-j{jobs}"
        for round_number in (1, 2, 3)
        for jobs in (1, 16)
    ]
    rows = [parse_case(args.artifacts, case) for case in cases]
    with (args.artifacts / "results.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    print("validated 6/6 points")


if __name__ == "__main__":
    main()
