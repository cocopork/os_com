#!/usr/bin/env python3
"""Deterministic NVLog points matching the existing mixed-sync matrix."""

from __future__ import annotations

import argparse
from typing import NamedTuple


DEVICES = ("nvme", "sata")
READ_PERCENTAGES = (0, 30, 50, 70)
SYNC_PERCENTAGES = (0, 20, 40, 60, 80, 100)
REPEATS = (1, 2, 3)


class Point(NamedTuple):
    device: str
    read_percent: int
    sync_percent: int
    repeat: int
    mode: str = "nvlog"
    threads: int = 8
    file_mib: int = 320
    dataset_mib: int = 2560
    seed: int = 20261119
    warmup_seconds: int = 10
    runtime_seconds: int = 30
    dram_cache_mib: int = 128
    nv_data_cache_mib: int = 128

    @property
    def case(self) -> str:
        return (
            f"mixed-{self.device}-nvlog-r{self.read_percent}w{100-self.read_percent}-"
            f"sync{self.sync_percent}-r{self.repeat}"
        )

    @property
    def file_bytes(self) -> int:
        return self.file_mib * 1024 * 1024

    @property
    def virtqueues(self) -> int:
        return 8 if self.device == "nvme" else 1


def matrix() -> list[Point]:
    return [
        Point(device, read_percent, sync_percent, repeat)
        for repeat in REPEATS
        for device in DEVICES
        for read_percent in READ_PERCENTAGES
        for sync_percent in SYNC_PERCENTAGES
    ]


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
                    point.read_percent,
                    point.sync_percent,
                    point.repeat,
                    point.threads,
                    point.file_mib,
                    point.file_bytes,
                    point.dataset_mib,
                    point.seed,
                    point.warmup_seconds,
                    point.runtime_seconds,
                    point.virtqueues,
                    point.dram_cache_mib,
                    point.nv_data_cache_mib,
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
    args = parser.parse_args()
    if args.command == "list":
        for point in matrix():
            print_point(point)
    else:
        print_point(point_by_case(args.case))


if __name__ == "__main__":
    main()
