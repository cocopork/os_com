#!/usr/bin/env python3
"""Defines the 4 KiB NVMe randwrite direct-submit ablation matrix."""

import argparse
from typing import NamedTuple


MODES = ("8q-8w-staged", "8q-direct")
THREADS = (1, 2, 4, 8, 16, 24, 32)


class Point(NamedTuple):
    mode: str
    jobs: int
    bs_kib: int = 4
    total_mib: int = 2048
    repeats: int = 3
    iothreads: int = 8

    @property
    def virtqueues(self) -> int:
        return 8

    @property
    def per_job_mib(self) -> int:
        return self.total_mib // self.jobs

    @property
    def actual_mib(self) -> int:
        return self.per_job_mib * self.jobs

    @property
    def expected_bytes(self) -> int:
        return self.actual_mib * 1024 * 1024

    @property
    def case(self) -> str:
        return f"raw-nvme-randwrite-{self.mode}-j{self.jobs}"


def matrix() -> list[Point]:
    return [Point(mode, jobs) for mode in MODES for jobs in THREADS]


def point(mode: str, jobs: int) -> Point:
    return next(item for item in matrix() if item.mode == mode and item.jobs == jobs)


def print_point(item: Point) -> None:
    print("\t".join(map(str, (
        item.case, item.mode, item.jobs, f"{item.bs_kib}k",
        item.per_job_mib, item.actual_mib, item.expected_bytes,
        item.repeats, item.virtqueues, item.iothreads,
    ))))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("list")
    config = subcommands.add_parser("config")
    config.add_argument("mode", choices=MODES)
    config.add_argument("jobs", type=int, choices=THREADS)
    args = parser.parse_args()
    if args.command == "list":
        for item in matrix():
            print_point(item)
    else:
        print_point(point(args.mode, args.jobs))


if __name__ == "__main__":
    main()
