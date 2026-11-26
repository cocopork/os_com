#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validate and summarize paired rNVPCe create/overwrite experiments."""

import argparse
import csv
import json
import re
from pathlib import Path


MIB = 1024 * 1024
CASES = {
    f"rnvpce-write-128k-{semantics}-j{jobs}": (semantics, jobs)
    for semantics in ("create", "overwrite")
    for jobs in (1, 16)
}
METRIC_RE = re.compile(
    r"\[PIPELINE\] name=(\S+) count=(\d+) total_ns=(\d+) max_ns=(\d+) "
    r"units=(\d+) bins=\[([0-9,]+)\]"
)
LATENCY_RE = re.compile(
    r"\[LATENCY\] (\S+) count=(\d+) sum_ns=(\d+) max_ns=(\d+)"
)


def clean_serial(text: str) -> str:
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\[[0-9;]*(?=\s)", "", text)
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


def parse_metrics(text: str) -> dict[str, dict[str, int]]:
    metrics = {}
    for match in METRIC_RE.finditer(text):
        name, count, total_ns, max_ns, units, bins = match.groups()
        histogram = [int(value) for value in bins.split(",")]
        if sum(histogram) != int(count):
            raise ValueError(f"{name}: histogram count mismatch")
        metrics[name] = {
            "count": int(count),
            "total_ns": int(total_ns),
            "max_ns": int(max_ns),
            "units": int(units),
        }
    return metrics


def parse_latency(text: str) -> list[dict[str, int | str]]:
    return [
        {
            "metric": name,
            "count": int(count),
            "total_ns": int(total_ns),
            "max_ns": int(max_ns),
        }
        for name, count, total_ns, max_ns in LATENCY_RE.findall(text)
    ]


def parse_case(artifacts: Path, case: str) -> tuple[dict, list[dict], list[dict]]:
    if case not in CASES:
        raise ValueError(f"unknown case: {case}")
    semantics, jobs = CASES[case]
    expected_bytes = (2048 // jobs) * jobs * MIB
    prepare = int(semantics == "overwrite")
    path = artifacts / f"{case}.log"
    if not path.is_file():
        raise ValueError(f"{case}: serial log missing")
    text = clean_serial(path.read_text(errors="replace"))
    config = (
        f"OVERWRITE_CONFIG case={case} prepare={prepare} bs=128k jobs={jobs} "
        f"total_mib=2048 expected_bytes={expected_bytes}"
    )
    if config not in text:
        raise ValueError(f"{case}: configuration marker missing")
    prepare_bytes = expected_bytes if prepare else 0
    if f"OVERWRITE_PREPARE_COMPLETE {case} bytes={prepare_bytes} status=0" not in text:
        raise ValueError(f"{case}: preparation marker missing")
    start_marker = f"OVERWRITE_FIO_START {case}"
    end_marker = f"OVERWRITE_FIO_END {case}"
    start = text.index(start_marker) + len(start_marker)
    end = text.index(end_marker, start)
    payload = fio_payload(text[start:end])
    if len(payload.get("jobs", [])) != 1:
        raise ValueError(f"{case}: expected one group-reported fio job")
    job = payload["jobs"][0]
    options = job.get("job options", {})
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
    write = job.get("write", {})
    if int(job.get("error", -1)) or fio_status or umount_status:
        raise ValueError(f"{case}: fio, sync, or unmount failed")
    if int(write.get("io_bytes", -1)) != expected_bytes:
        raise ValueError(f"{case}: fio byte count is incomplete")
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{case}: invalid timing order")
    check = artifacts / f"{case}-e2fsck.log"
    check_text = check.read_text(errors="replace") if check.is_file() else ""
    if "Pass 5: Checking group summary information" not in check_text or not re.search(
        r"OVERWRITE_E2FSCK_STATUS 0(?:\s|$)", check_text
    ):
        raise ValueError(f"{case}: e2fsck failed or incomplete")
    elapsed = (sync_end_ns - start_ns) / 1e9
    row = {
        "case": case,
        "semantics": semantics,
        "jobs": jobs,
        "io_mib": expected_bytes / MIB,
        "fio_mib_s": int(write["bw_bytes"]) / MIB,
        "sync_mib_s": expected_bytes / MIB / elapsed,
        "fio_seconds": (fio_end_ns - start_ns) / 1e9,
        "sync_seconds": (sync_end_ns - fio_end_ns) / 1e9,
    }
    metric_rows = [
        {"case": case, "metric": name, **values}
        for name, values in sorted(parse_metrics(text).items())
    ]
    latency_rows = [
        {"case": case, **values}
        for values in parse_latency(text)
    ]
    return row, metric_rows, latency_rows


def write_csv(path: Path, rows: list[dict]) -> None:
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    validate = subparsers.add_parser("validate")
    validate.add_argument("artifacts", type=Path)
    validate.add_argument("case", choices=CASES)
    summarize = subparsers.add_parser("summarize")
    summarize.add_argument("artifacts", type=Path)
    args = parser.parse_args()
    if args.command == "validate":
        parse_case(args.artifacts, args.case)
        print(f"{args.case}: complete")
        return
    rows = []
    metrics = []
    latency = []
    for case in CASES:
        row, case_metrics, case_latency = parse_case(args.artifacts, case)
        rows.append(row)
        metrics.extend(case_metrics)
        latency.extend(case_latency)
    write_csv(args.artifacts / "results.csv", rows)
    if metrics:
        write_csv(args.artifacts / "pipeline-metrics.csv", metrics)
    if latency:
        write_csv(args.artifacts / "latency-metrics.csv", latency)
    print("validated 4/4 points")


if __name__ == "__main__":
    main()
