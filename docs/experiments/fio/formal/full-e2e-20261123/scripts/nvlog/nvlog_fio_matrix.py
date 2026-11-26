#!/usr/bin/env python3
"""Define and validate the NVLog fio matrix matching the rNVPCe-SU sweep."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
from typing import NamedTuple


MIB = 1024 * 1024
DEVICES = ("nvme", "sata")
WORKLOADS = ("overwrite-randwrite", "append-write", "cold-randread")
THREADS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
BLOCK_KIB = (4, 8, 16, 32, 64, 128, 256, 512, 1024)
REPEATS = (1, 2, 3)


class Point(NamedTuple):
    device: str
    workload: str
    axis: str
    jobs: int
    bs_kib: int
    repeat: int
    total_mib: int = 2048
    runtime_seconds: int = 0
    file_mib: int = 0

    @property
    def per_job_mib(self) -> int:
        if self.file_mib:
            return self.file_mib
        if self.axis == "threads-reconfigured":
            return (self.total_mib + self.jobs - 1) // self.jobs
        return self.total_mib // self.jobs

    @property
    def actual_mib(self) -> int:
        return self.per_job_mib * self.jobs

    @property
    def expected_bytes(self) -> int:
        if self.runtime_seconds:
            return 0
        return self.actual_mib * MIB

    @property
    def bs(self) -> str:
        return f"{self.bs_kib}k"

    @property
    def value(self) -> str:
        return str(self.jobs) if self.axis.startswith("threads") else self.bs

    @property
    def case(self) -> str:
        return (
            f"e2e-{self.device}-nvlog-{self.workload}-"
            f"{self.axis}-{self.value}-r{self.repeat}"
        )

    @property
    def fio_rw(self) -> str:
        return {
            "overwrite-randwrite": "randwrite",
            "append-write": "write",
            "cold-randread": "randread",
        }[self.workload]

    @property
    def fsync_blocks(self) -> int:
        return 0 if self.workload == "cold-randread" else 1

    @property
    def prepare(self) -> bool:
        return self.workload != "append-write"


def matrix() -> list[Point]:
    points = []
    for repeat in REPEATS:
        for device in DEVICES:
            for workload in WORKLOADS:
                points.extend(
                    Point(device, workload, "threads", jobs, 4, repeat)
                    for jobs in THREADS
                )
                points.extend(
                    Point(device, workload, "bs", 1, bs_kib, repeat)
                    for bs_kib in BLOCK_KIB
                )
    return points


def reconfigured_thread_matrix() -> list[Point]:
    points = []
    for repeat in REPEATS:
        for device in DEVICES:
            for jobs in THREADS:
                points.append(Point(
                    device, "append-write", "threads-reconfigured", jobs, 4,
                    repeat, total_mib=0, file_mib=1024,
                ))
                points.append(Point(
                    device, "overwrite-randwrite", "threads-reconfigured", jobs, 4,
                    repeat, total_mib=0, file_mib=1024,
                ))
                points.append(Point(
                    device, "cold-randread", "threads-reconfigured", jobs, 4,
                    repeat, total_mib=2560,
                ))
    return points


def point_by_case(case: str) -> Point:
    try:
        return next(
            point for point in (*matrix(), *reconfigured_thread_matrix())
            if point.case == case
        )
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def parse_result(point_dir: Path, point: Point) -> dict:
    raw = point_dir.joinpath("qemu.log").read_text(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)
    completion = re.search(
        rf"^FULL_E2E_COMPLETE case={re.escape(point.case)} (.*)$", raw, re.M
    )
    if not completion:
        raise ValueError(f"{point.case}: missing completion marker")
    fields = dict(re.findall(r"(\w+)=([^\s]+)", completion.group(1)))
    for name in ("fio_status", "sync_status", "close_status", "umount_status"):
        if fields.get(name) != "0":
            raise ValueError(f"{point.case}: {name} is not zero")
    if re.search(r"^FULL_E2E_MEMORY_EVENT_OOM_KILL=[1-9]\d*$", raw, re.M):
        raise ValueError(f"{point.case}: benchmark cgroup was OOM-killed")
    memory_events = {
        name.lower(): int(value)
        for name, value in re.findall(
            r"^FULL_E2E_MEMORY_EVENT_([A-Z_]+)=(\d+)$", raw, re.M
        )
    }
    cache_samples = [
        {name: int(value) for name, value in re.findall(
            r"(anon|file|file_dirty|file_writeback)=(\d+)", line
        )}
        for line in raw.splitlines()
        if line.startswith("FULL_E2E_CACHE_SAMPLE ")
    ]
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
    measured_bytes = int(measured.get("io_bytes", -1))
    if point.runtime_seconds and measured_bytes <= 0:
        raise ValueError(f"{point.case}: fio completed no I/O")
    if not point.runtime_seconds and measured_bytes != point.expected_bytes:
        raise ValueError(f"{point.case}: fio byte count is incomplete")
    if int(point_dir.joinpath("e2fsck.status").read_text()) != 0:
        raise ValueError(f"{point.case}: e2fsck failed")
    usage = re.findall(r"^nvpcctl: used\s+(\d+) pages$", raw, re.M)
    if not usage:
        raise ValueError(f"{point.case}: missing NVLog usage")
    nvlog_used_pages = max(map(int, usage))
    if point.fsync_blocks and nvlog_used_pages <= 0:
        raise ValueError(f"{point.case}: synchronous write did not use NVLog")
    start_ns = int(fields["start_ns"])
    fio_end_ns = int(fields["fio_end_ns"])
    sync_end_ns = int(fields["sync_end_ns"])
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{point.case}: invalid timing order")
    elapsed = (sync_end_ns - start_ns) / 1e9
    return {
        "case": point.case,
        "device": point.device,
        "mode": "nvlog",
        "workload": point.workload,
        "axis": point.axis,
        "value": point.value,
        "bs_kib": point.bs_kib,
        "jobs": point.jobs,
        "repeat": point.repeat,
        "actual_mib": measured_bytes / MIB,
        "configured_dram_cache_mib": 128,
        "configured_nv_data_cache_mib": 128,
        "configured_total_data_cache_mib": 256,
        "fio_fsync_blocks": point.fsync_blocks,
        "nvlog_used_pages": nvlog_used_pages,
        "cache_control": "memcg-memory-high",
        "cache_file_peak_bytes": max(
            (sample.get("file", 0) for sample in cache_samples), default=0
        ),
        "cache_anon_peak_bytes": max(
            (sample.get("anon", 0) for sample in cache_samples), default=0
        ),
        "memory_high_mib": 136,
        "memory_max_mib": 512,
        "memory_events": memory_events,
        "fio_mib_s": int(measured["bw_bytes"]) / MIB,
        "including_sync_mib_s": None if operation == "read" else measured_bytes / MIB / elapsed,
        "iops": float(measured["iops"]),
        "mean_latency_us": float(measured["lat_ns"]["mean"]) / 1000,
        "p99_latency_us": float(
            measured["clat_ns"].get("percentile", {}).get("99.000000", 0)
        ) / 1000,
    }


def print_point(point: Point) -> None:
    print("\t".join(map(str, (
        point.case, point.device, point.workload, point.axis, point.bs,
        point.jobs, point.per_job_mib, point.expected_bytes,
        point.fio_rw, int(point.prepare), point.repeat, point.runtime_seconds,
        point.fsync_blocks,
    ))))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    commands.add_parser("reconfigured-thread-list")
    config = commands.add_parser("config")
    config.add_argument("case")
    parse = commands.add_parser("parse")
    parse.add_argument("point_dir", type=Path)
    parse.add_argument("case")
    args = parser.parse_args()
    if args.command in ("list", "reconfigured-thread-list"):
        points = matrix() if args.command == "list" else reconfigured_thread_matrix()
        for point in points:
            print_point(point)
    elif args.command == "config":
        print_point(point_by_case(args.case))
    else:
        print(json.dumps(parse_result(args.point_dir, point_by_case(args.case)), indent=2))


if __name__ == "__main__":
    main()
