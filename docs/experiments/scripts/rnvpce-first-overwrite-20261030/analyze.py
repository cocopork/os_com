#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validate and summarize rNVPCe first-write and overwrite measurements."""

import argparse
import csv
import json
import re
from pathlib import Path

MIB = 1024 * 1024
CASES = {
    f"rnvpce-{rw}-{bs}-{semantics}-j{jobs}": (rw, bs, semantics, jobs)
    for rw, bs in (("randwrite", "4k"), ("write", "128k"))
    for semantics in ("first", "overwrite")
    for jobs in (1, 16)
}


def clean_serial(text: str) -> str:
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    return re.sub(r"\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*", "", text)


def fio_payload(body: str) -> dict:
    body = re.sub(r"(?<=\d)m(?=[,\s])", "", body)
    body = re.sub(r'(?m)^(\s*)[0-9;m\[]+(\s+")', r"\1\2", body)
    for match in re.finditer(r"\{", body):
        try:
            value = json.JSONDecoder().raw_decode(body[match.start():])[0]
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "jobs" in value:
            return value
    raise ValueError("fio JSON not found")


def parse_case(artifacts: Path, case: str) -> dict:
    rw, bs, semantics, jobs = CASES[case]
    expected_bytes = (2048 // jobs) * jobs * MIB
    prepare = int(semantics == "overwrite")
    text = clean_serial((artifacts / f"{case}.log").read_text(errors="replace"))
    config = (
        f"PERF_CONFIG case={case} prepare={prepare} rw={rw} bs={bs} jobs={jobs} "
        f"total_mib=2048 expected_bytes={expected_bytes}"
    )
    if config not in text:
        raise ValueError(f"{case}: configuration marker missing")
    prepared = expected_bytes if prepare else 0
    if f"PERF_PREPARE_COMPLETE {case} bytes={prepared} status=0" not in text:
        raise ValueError(f"{case}: preparation marker missing")
    start = text.index(f"PERF_FIO_START {case}")
    end = text.index(f"PERF_FIO_END {case}", start)
    payload = fio_payload(text[start:end])
    job = payload["jobs"][0]
    options = job["job options"]
    for key, expected in (("rw", rw), ("bs", bs), ("numjobs", str(jobs)), ("direct", "0")):
        if str(options.get(key)) != expected:
            raise ValueError(f"{case}: {key} is not {expected}")
    completion = re.search(
        rf"PERF_COMPLETE {re.escape(case)} fio_status=(\d+) start_ns=(\d+) "
        r"fio_end_ns=(\d+) sync_end_ns=(\d+) umount_status=(\d+)", text
    )
    if not completion:
        raise ValueError(f"{case}: completion marker missing")
    fio_status, start_ns, fio_end_ns, sync_end_ns, umount_status = map(int, completion.groups())
    write = job["write"]
    if int(job.get("error", -1)) or fio_status or umount_status:
        raise ValueError(f"{case}: fio or unmount failed")
    if int(write["io_bytes"]) != expected_bytes:
        raise ValueError(f"{case}: incomplete byte count")
    check = (artifacts / f"{case}-e2fsck.log").read_text(errors="replace")
    if "Pass 5: Checking group summary information" not in check or "PERF_E2FSCK_STATUS 0" not in check:
        raise ValueError(f"{case}: e2fsck failed")
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": case,
        "workload": rw,
        "bs": bs,
        "semantics": semantics,
        "jobs": jobs,
        "fio_mib_s": round(int(write["bw_bytes"]) / MIB, 2),
        "durable_mib_s": round(expected_bytes / MIB / elapsed, 2),
        "fio_seconds": round((fio_end_ns - start_ns) / 1e9, 3),
        "sync_seconds": round((sync_end_ns - fio_end_ns) / 1e9, 3),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    validate = sub.add_parser("validate")
    validate.add_argument("artifacts", type=Path)
    validate.add_argument("case", choices=CASES)
    summarize = sub.add_parser("summarize")
    summarize.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    if args.command == "validate":
        parse_case(args.artifacts, args.case)
        print(f"{args.case}: complete")
        return
    rows = [parse_case(args.artifacts, case) for case in CASES]
    with (args.artifacts / "results.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    print(f"validated {len(rows)}/{len(CASES)} points")


if __name__ == "__main__":
    main()
