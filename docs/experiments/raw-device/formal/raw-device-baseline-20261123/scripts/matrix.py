#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Defines the filesystem-free Linux/Asterinas raw-device fio matrix."""

import argparse
from typing import NamedTuple


MIB = 1024 * 1024
SYSTEMS = ("asterinas", "linux")
DEVICES = ("nvme", "sata")
WORKLOADS = ("append-write", "overwrite-randwrite", "cold-randread")
THREADS = (1, 4, 8, 12, 16, 20, 24, 28, 32)
BLOCK_KIB = (4, 8, 16, 32, 64, 128, 256, 512, 1024)


class Point(NamedTuple):
    device: str
    system: str
    workload: str
    axis: str
    jobs: int
    bs_kib: int
    total_mib: int = 2048
    repeats: int = 3

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
    def fio_rw(self) -> str:
        return {
            "append-write": "write",
            "overwrite-randwrite": "randwrite",
            "cold-randread": "randread",
        }[self.workload]

    @property
    def case(self) -> str:
        value = str(self.jobs) if self.axis == "threads" else f"{self.bs_kib}k"
        return f"raw-{self.device}-{self.system}-{self.workload}-{self.axis}-{value}"


def virtqueues(device: str) -> int:
    return {"nvme": 8, "sata": 1}[device]


def matrix() -> list[Point]:
    points = []
    for device in DEVICES:
        for system in SYSTEMS:
            for workload in WORKLOADS:
                points.extend(
                    Point(device, system, workload, "threads", jobs, 4)
                    for jobs in THREADS
                )
                points.extend(
                    Point(device, system, workload, "bs", 1, bs_kib)
                    for bs_kib in BLOCK_KIB
                )
    return points


def point_by_case(case: str) -> Point:
    try:
        return next(point for point in matrix() if point.case == case)
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def print_point(point: Point) -> None:
    print(
        "\t".join(
            map(
                str,
                (
                    point.case,
                    point.device,
                    point.system,
                    point.workload,
                    point.axis,
                    f"{point.bs_kib}k",
                    point.jobs,
                    point.per_job_mib,
                    point.actual_mib,
                    point.expected_bytes,
                    point.fio_rw,
                    point.repeats,
                    virtqueues(point.device),
                ),
            )
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("list")
    config = subcommands.add_parser("config")
    config.add_argument("case")
    args = parser.parse_args()
    if args.command == "list":
        for point in matrix():
            print_point(point)
    else:
        print_point(point_by_case(args.case))


if __name__ == "__main__":
    main()
