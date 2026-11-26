#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Define, validate, and summarize the fixed-layout overwrite sweep."""

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re


MIB = 1024 * 1024
MODES = ("ext4", "rnvpce")
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
        return str(self.jobs) if self.axis == "threads" else self.bs

    @property
    def case(self) -> str:
        return f"overwrite-{self.mode}-{self.axis}-{self.workload}-{self.value}"

    @property
    def disk_format(self) -> str:
        return "journal" if self.mode == "ext4" else "nojournal"


def matrix() -> list[Point]:
    points = []
    for workload in WORKLOADS:
        for jobs in THREADS:
            points.extend(Point(mode, "threads", workload, jobs, 4) for mode in MODES)
        for bs_kib in BLOCK_KIB:
            points.extend(Point(mode, "bs", workload, 1, bs_kib) for mode in MODES)
    return points


def validate_layout(rows: list[dict], expected_files: int, per_file_mib: int) -> dict:
    if len(rows) != expected_files:
        raise ValueError("layout file count does not match jobs")
    expected_size = per_file_mib * MIB
    if any(row.get("size_bytes") != expected_size for row in rows):
        raise ValueError("layout file size does not match the workload")
    # Sequential EXT4 allocation normally creates roughly one extent per block
    # group. This generous bound admits group boundaries but rejects the
    # thousands of 128 KiB interleaved extents produced by concurrent prepare.
    max_allowed = (per_file_mib + 63) // 64 + 8
    max_extents = max(row.get("data_extents", 0) for row in rows)
    if max_extents == 0 or max_extents > max_allowed:
        raise ValueError(
            f"prepared file is fragmented: {max_extents} extents exceeds {max_allowed}"
        )
    return {
        "files": expected_files,
        "per_file_mib": per_file_mib,
        "min_data_extents": min(row["data_extents"] for row in rows),
        "max_data_extents": max_extents,
        "total_extent_index_blocks": sum(row["extent_index_blocks"] for row in rows),
        "max_allowed_data_extents": max_allowed,
    }


def point_by_case(case: str) -> Point:
    try:
        return next(point for point in matrix() if point.case == case)
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def parse_result(point_dir: Path, point: Point) -> dict:
    raw = (point_dir / "qemu.log").read_bytes().decode(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)
    marker = re.search(r"^OVERWRITE_SWEEP_COMPLETE (.*)$", raw, re.M)
    if not marker:
        raise ValueError(f"{point.case}: missing completion marker")
    fields = dict(re.findall(r"(\w+)=([^\s]+)", marker.group(1)))
    for name in ("fio_status", "sync_status", "umount_status"):
        if fields.get(name) != "0":
            raise ValueError(f"{point.case}: {name} is not zero")
    expected_fields = {
        "case": point.case,
        "mode": point.mode,
        "axis": point.axis,
        "workload": point.workload,
        "bs": point.bs,
        "jobs": str(point.jobs),
        "expected_bytes": str(point.expected_bytes),
    }
    if any(fields.get(key) != value for key, value in expected_fields.items()):
        raise ValueError(f"{point.case}: configuration marker changed")
    payload = raw.split(f"OVERWRITE_SWEEP_FIO_START {point.case}\n", 1)[1].split(
        f"OVERWRITE_SWEEP_FIO_END {point.case}", 1
    )[0]
    fio, _ = json.JSONDecoder().raw_decode(payload[payload.index("{"):])
    if len(fio.get("jobs", [])) != 1:
        raise ValueError(f"{point.case}: grouped fio output is invalid")
    job = fio["jobs"][0]
    write = job["write"]
    if job["error"] != 0 or write["io_bytes"] != point.expected_bytes:
        raise ValueError(f"{point.case}: fio failed or byte count is incomplete")
    start_ns = int(fields["start_ns"])
    fio_end_ns = int(fields["fio_end_ns"])
    sync_end_ns = int(fields["sync_end_ns"])
    if not start_ns < fio_end_ns <= sync_end_ns:
        raise ValueError(f"{point.case}: invalid timing order")
    layout_rows = json.loads((point_dir / "layout.json").read_text())
    layout = validate_layout(layout_rows, point.jobs, point.per_job_mib)
    e2fsck = int((point_dir / "e2fsck.status").read_text())
    if e2fsck != 0:
        raise ValueError(f"{point.case}: e2fsck failed")
    total_mib = point.actual_mib
    return {
        "case": point.case,
        "mode": point.mode,
        "axis": point.axis,
        "workload": point.workload,
        "value": point.value,
        "bs_kib": point.bs_kib,
        "jobs": point.jobs,
        "actual_mib": total_mib,
        "fio_mib_s": write["bw_bytes"] / MIB,
        "including_sync_mib_s": total_mib / ((sync_end_ns - start_ns) / 1e9),
        "iops": write["iops"],
        "mean_latency_us": write["lat_ns"]["mean"] / 1000,
        "p99_latency_us": write["clat_ns"].get("percentile", {}).get("99.000000", 0) / 1000,
        "layout": layout,
        "hctx": json.loads((point_dir / "hctx-counts.json").read_text()),
    }


def print_point(point: Point) -> None:
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
                    point.per_job_mib,
                    point.expected_bytes,
                    point.disk_format,
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
    parse = commands.add_parser("parse")
    parse.add_argument("point_dir", type=Path)
    parse.add_argument("case")
    args = parser.parse_args()
    if args.command == "list":
        for point in matrix():
            print_point(point)
    elif args.command == "config":
        print_point(point_by_case(args.case))
    else:
        print(json.dumps(parse_result(args.point_dir, point_by_case(args.case)), indent=2))


if __name__ == "__main__":
    main()
