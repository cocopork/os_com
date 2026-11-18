#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Defines and validates the two-device full end-to-end fio sweep."""

import argparse
import json
from pathlib import Path
import re
from typing import NamedTuple


MIB = 1024 * 1024
DEVICES = ("nvme", "sata")
MODES = ("ext4", "rnvpce")
WORKLOADS = ("overwrite-randwrite", "append-write", "cold-randread")
THREADS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
BLOCK_KIB = (4, 8, 16, 32, 64, 128, 256, 512, 1024)
REPEATS = (1, 2, 3)


class Point(NamedTuple):
    device: str
    mode: str
    workload: str
    axis: str
    jobs: int
    bs_kib: int
    repeat: int
    total_mib: int = 2048

    @property
    def per_job_mib(self) -> int:
        return self.total_mib // self.jobs

    @property
    def actual_mib(self) -> int:
        return self.per_job_mib * self.jobs

    @property
    def expected_bytes(self) -> int:
        return self.actual_mib * MIB

    @property
    def bs(self) -> str:
        return f"{self.bs_kib}k"

    @property
    def value(self) -> str:
        if self.axis == "threads":
            return str(self.jobs)
        if self.axis == "pagecache-regression":
            return f"{self.bs}-j{self.jobs}"
        return self.bs

    @property
    def case(self) -> str:
        return (
            f"e2e-{self.device}-{self.mode}-{self.workload}-"
            f"{self.axis}-{self.value}-r{self.repeat}"
        )

    @property
    def disk_format(self) -> str:
        return "journal" if self.mode == "ext4" else "nojournal"

    @property
    def fio_rw(self) -> str:
        return {
            "overwrite-randwrite": "randwrite",
            "append-write": "write",
            "cold-randread": "randread",
        }[self.workload]

    @property
    def prepare(self) -> bool:
        return self.workload != "append-write"


def matrix() -> list[Point]:
    points = []
    for repeat in REPEATS:
        for device in DEVICES:
            for workload in WORKLOADS:
                for jobs in THREADS:
                    points.extend(
                        Point(device, mode, workload, "threads", jobs, 4, repeat)
                        for mode in MODES
                    )
                for bs_kib in BLOCK_KIB:
                    points.extend(
                        Point(device, mode, workload, "bs", 1, bs_kib, repeat)
                        for mode in MODES
                    )
    return points


def pagecache_regression_matrix() -> list[Point]:
    """Focused performance-regression screen for the PageCache integration."""
    coordinates = (
        ("append-write", 128, 1),
        ("append-write", 128, 16),
        ("overwrite-randwrite", 4, 1),
        ("overwrite-randwrite", 4, 16),
        ("cold-randread", 4, 1),
        ("cold-randread", 16, 1),
        ("cold-randread", 64, 1),
        ("cold-randread", 256, 1),
    )
    return [
        Point("nvme", "rnvpce", workload, "pagecache-regression", jobs, bs_kib, repeat)
        for repeat in REPEATS
        for workload, bs_kib, jobs in coordinates
    ]


def point_by_case(case: str) -> Point:
    try:
        return next(
            point
            for point in (*matrix(), *pagecache_regression_matrix())
            if point.case == case
        )
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def parse_result(point_dir: Path, point: Point) -> dict:
    raw = (point_dir / "qemu.log").read_bytes().decode(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)
    completion = re.search(rf"^FULL_E2E_COMPLETE case={re.escape(point.case)} (.*)$", raw, re.M)
    if not completion:
        raise ValueError(f"{point.case}: missing completion marker")
    fields = dict(re.findall(r"(\w+)=([^\s]+)", completion.group(1)))
    for name in ("fio_status", "sync_status", "umount_status"):
        if fields.get(name) != "0":
            raise ValueError(f"{point.case}: {name} is not zero")
    begin = f"FULL_E2E_FIO_START {point.case}\n"
    end = f"FULL_E2E_FIO_END {point.case}"
    if begin not in raw or end not in raw:
        raise ValueError(f"{point.case}: incomplete fio markers")
    payload = raw.split(begin, 1)[1].split(end, 1)[0]
    fio, _ = json.JSONDecoder().raw_decode(payload[payload.index("{"):])
    if len(fio.get("jobs", [])) != 1 or fio["jobs"][0].get("error") != 0:
        raise ValueError(f"{point.case}: fio failed")
    operation = "read" if point.fio_rw == "randread" else "write"
    measured = fio["jobs"][0][operation]
    if int(measured.get("io_bytes", -1)) != point.expected_bytes:
        raise ValueError(f"{point.case}: fio byte count is incomplete")
    if int((point_dir / "e2fsck.status").read_text()) != 0:
        raise ValueError(f"{point.case}: e2fsck failed")
    start_ns = int(fields["start_ns"])
    fio_end_ns = int(fields["fio_end_ns"])
    sync_end_ns = int(fields["sync_end_ns"])
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{point.case}: invalid timing order")
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": point.case,
        "device": point.device,
        "mode": point.mode,
        "workload": point.workload,
        "axis": point.axis,
        "value": point.value,
        "bs_kib": point.bs_kib,
        "jobs": point.jobs,
        "repeat": point.repeat,
        "actual_mib": point.actual_mib,
        "fio_mib_s": int(measured["bw_bytes"]) / MIB,
        "including_sync_mib_s": None if operation == "read" else point.actual_mib / elapsed,
        "iops": float(measured["iops"]),
        "mean_latency_us": float(measured["lat_ns"]["mean"]) / 1000,
        "p99_latency_us": float(measured["clat_ns"].get("percentile", {}).get("99.000000", 0)) / 1000,
    }


def print_point(point: Point) -> None:
    print(
        "\t".join(
            map(
                str,
                (
                    point.case,
                    point.device,
                    point.mode,
                    point.workload,
                    point.axis,
                    point.bs,
                    point.jobs,
                    point.per_job_mib,
                    point.expected_bytes,
                    point.disk_format,
                    point.fio_rw,
                    int(point.prepare),
                    point.repeat,
                ),
            )
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    commands.add_parser("pagecache-regression-list")
    config = commands.add_parser("config")
    config.add_argument("case")
    parse = commands.add_parser("parse")
    parse.add_argument("point_dir", type=Path)
    parse.add_argument("case")
    args = parser.parse_args()
    if args.command in ("list", "pagecache-regression-list"):
        points = matrix() if args.command == "list" else pagecache_regression_matrix()
        for point in points:
            print_point(point)
    elif args.command == "config":
        print_point(point_by_case(args.case))
    else:
        print(json.dumps(parse_result(args.point_dir, point_by_case(args.case)), indent=2))


if __name__ == "__main__":
    main()
