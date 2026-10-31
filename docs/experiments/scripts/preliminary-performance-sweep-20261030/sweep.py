#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Define, validate, and summarize the preliminary performance sweep."""

import argparse
import csv
import json
import re
from dataclasses import dataclass
from pathlib import Path


MIB = 1024 * 1024
MODES = ("ext4", "rnvpc", "rnvpce")
WORKLOADS = ("randwrite", "write")
THREADS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
BLOCK_KIB = (4, 8, 16, 32, 64, 128, 256, 512, 1024)


@dataclass(frozen=True)
class Point:
    mode: str
    axis: str
    workload: str
    jobs: int
    bs_kib: int
    total_mib: int = 2048

    @property
    def bs(self) -> str:
        return f"{self.bs_kib}k"

    @property
    def value(self) -> str:
        return str(self.jobs) if self.axis == "threads" else self.bs

    @property
    def case(self) -> str:
        return f"prelim-{self.mode}-{self.axis}-{self.workload}-{self.value}"

    @property
    def per_job_mib(self) -> int:
        return self.total_mib // self.jobs

    @property
    def expected_bytes(self) -> int:
        return self.per_job_mib * self.jobs * MIB

    @property
    def disk_journal(self) -> bool:
        return self.mode == "ext4"

    @property
    def runtime_journal(self) -> bool:
        return self.mode == "ext4"


def matrix() -> list[Point]:
    points = []
    for mode in MODES:
        for workload in WORKLOADS:
            points.extend(Point(mode, "threads", workload, jobs, 4) for jobs in THREADS)
            points.extend(Point(mode, "bs", workload, 1, bs_kib) for bs_kib in BLOCK_KIB)
    return points


def point_by_case(case: str) -> Point:
    try:
        return next(point for point in matrix() if point.case == case)
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def _clean_serial(text: str) -> str:
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\[[0-9;]*(?=\s)", "", text)
    return re.sub(r"\[CPU-ISOLATION\] role=\w+ cpu=\d+\s*", "", text)


def _fio_payload(text: str, point: Point) -> dict:
    start_marker = f"PRELIM_FIO_START {point.case}"
    end_marker = f"PRELIM_FIO_END {point.case}"
    try:
        start = text.index(start_marker) + len(start_marker)
        end = text.index(end_marker, start)
    except ValueError as error:
        raise ValueError(f"{point.case}: fio markers incomplete") from error
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
    raise ValueError(f"{point.case}: fio JSON not found")


def _require(pattern: str, text: str, point: Point, label: str) -> re.Match:
    match = re.search(pattern, text)
    if not match:
        raise ValueError(f"{point.case}: missing or inconsistent {label}")
    return match


def parse_log(path: Path, point: Point) -> dict:
    if not path.is_file():
        raise ValueError(f"{point.case}: serial log missing")
    text = _clean_serial(path.read_text(errors="replace"))
    _require(
        rf"PRELIM_CONFIG case={re.escape(point.case)} mode={point.mode} axis={point.axis} "
        rf"workload={point.workload} bs={point.bs} jobs={point.jobs} "
        rf"total_mib={point.total_mib} expected_bytes={point.expected_bytes}(?:\s|$)",
        text,
        point,
        "configuration marker",
    )
    _require(
        rf"PRELIM_JOURNAL case={re.escape(point.case)} disk={int(point.disk_journal)} "
        rf"runtime={int(point.runtime_journal)}(?:\s|$)",
        text,
        point,
        "journal marker",
    )
    prepare_bytes = point.expected_bytes if point.workload == "randwrite" else 0
    _require(
        rf"PRELIM_PREPARE_COMPLETE {re.escape(point.case)} bytes={prepare_bytes} status=0(?:\s|$)",
        text,
        point,
        "preparation marker",
    )
    payload = _fio_payload(text, point)
    if len(payload.get("jobs", [])) != 1:
        raise ValueError(f"{point.case}: expected one group-reported fio job")
    job = payload["jobs"][0]
    options = job.get("job options", {})
    expected_options = {
        "rw": point.workload,
        "bs": point.bs,
        "numjobs": str(point.jobs),
        "direct": "0",
        "ioengine": "psync",
    }
    for key, expected in expected_options.items():
        if str(options.get(key)) != expected:
            raise ValueError(f"{point.case}: fio option {key} is not {expected}")
    completion = _require(
        rf"PRELIM_COMPLETE {re.escape(point.case)} fio_status=(\d+) start_ns=(\d+) "
        r"fio_end_ns=(\d+) sync_end_ns=(\d+) umount_status=(\d+)(?:\s|$)",
        text,
        point,
        "completion marker",
    )
    fio_status, start_ns, fio_end_ns, sync_end_ns, umount_status = map(
        int, completion.groups()
    )
    write = job.get("write", {})
    if int(job.get("error", -1)) != 0 or fio_status != 0 or umount_status != 0:
        raise ValueError(f"{point.case}: fio, sync, or unmount failed")
    if int(write.get("io_bytes", -1)) != point.expected_bytes:
        raise ValueError(f"{point.case}: fio byte count is incomplete")
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{point.case}: invalid timing order")
    clat = write.get("clat_ns", {})
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": point.case,
        "mode": point.mode,
        "axis": point.axis,
        "workload": point.workload,
        "value": point.value,
        "bs": point.bs,
        "jobs": point.jobs,
        "expected_bytes": point.expected_bytes,
        "io_mib": point.expected_bytes / MIB,
        "fio_mib_s": int(write["bw_bytes"]) / MIB,
        "sync_mib_s": point.expected_bytes / MIB / elapsed,
        "fio_seconds": (fio_end_ns - start_ns) / 1e9,
        "sync_seconds": (sync_end_ns - fio_end_ns) / 1e9,
        "mean_us": float(clat.get("mean", 0)) / 1000,
        "p99_us": float(clat.get("percentile", {}).get("99.000000", 0)) / 1000,
        "disk_journal": int(point.disk_journal),
        "runtime_journal": int(point.runtime_journal),
        "outcome": "complete",
    }


def parse_artifacts(artifacts: Path, point: Point) -> dict:
    row = parse_log(artifacts / f"{point.case}.log", point)
    check_path = artifacts / f"{point.case}-e2fsck.log"
    if not check_path.is_file():
        raise ValueError(f"{point.case}: e2fsck log missing")
    check = check_path.read_text(errors="replace")
    if "Pass 5: Checking group summary information" not in check:
        raise ValueError(f"{point.case}: e2fsck did not complete")
    if not re.search(r"PRELIM_E2FSCK_STATUS 0(?:\s|$)", check):
        raise ValueError(f"{point.case}: e2fsck failed")
    return row


def write_csv(path: Path, rows: list[dict]) -> None:
    if not rows:
        raise ValueError("no validated rows")
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def write_markdown(path: Path, rows: list[dict]) -> None:
    lines = [
        "| Mode | Axis | Workload | Value | fio MiB/s | sync MiB/s |",
        "|---|---|---|---:|---:|---:|",
    ]
    lines.extend(
        f"| {row['mode']} | {row['axis']} | {row['workload']} | {row['value']} "
        f"| {row['fio_mib_s']:.2f} | {row['sync_mib_s']:.2f} |"
        for row in rows
    )
    path.write_text("\n".join(lines) + "\n")


def _print_point(point: Point) -> None:
    print(
        "\t".join(
            map(
                str,
                (
                    point.case,
                    point.mode,
                    point.axis,
                    point.workload,
                    point.bs,
                    point.jobs,
                    point.total_mib,
                    point.expected_bytes,
                    int(point.disk_journal),
                    int(point.runtime_journal),
                ),
            )
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    config = commands.add_parser("config")
    config.add_argument("case")
    validate = commands.add_parser("validate")
    validate.add_argument("artifacts", type=Path)
    validate.add_argument("case")
    summarize = commands.add_parser("summarize")
    summarize.add_argument("artifacts", type=Path)
    summarize.add_argument("--allow-partial", action="store_true")
    args = parser.parse_args()
    if args.command == "list":
        for point in matrix():
            _print_point(point)
    elif args.command == "config":
        _print_point(point_by_case(args.case))
    elif args.command == "validate":
        row = parse_artifacts(args.artifacts, point_by_case(args.case))
        print(f"{row['case']}: complete")
    else:
        rows = []
        errors = []
        for point in matrix():
            try:
                rows.append(parse_artifacts(args.artifacts, point))
            except ValueError as error:
                errors.append(str(error))
        if errors and not args.allow_partial:
            raise ValueError("\n".join(errors))
        write_csv(args.artifacts / "results.csv", rows)
        write_markdown(args.artifacts / "results.md", rows)
        print(f"validated {len(rows)}/{len(matrix())} points")
        for error in errors:
            print(error)


if __name__ == "__main__":
    main()
