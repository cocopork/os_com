#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Defines the deterministic mixed write+fsync experiment matrix."""

import argparse
from typing import NamedTuple


DEVICES = ("nvme", "sata")
MODES = ("ext2", "ext4", "rnvpce")
READ_PERCENTAGES = (0, 30, 50, 70)
SYNC_PERCENTAGES = (0, 20, 40, 60, 80, 100)
REPEATS = (1, 2, 3)
THREADS = 8
FILE_MIB = 320
DATASET_MIB = THREADS * FILE_MIB
SEED = 20261119
WARMUP_SECONDS = 10
RUNTIME_SECONDS = 30


class Point(NamedTuple):
    device: str
    mode: str
    read_percent: int
    sync_percent: int
    repeat: int
    threads: int = THREADS
    file_mib: int = FILE_MIB
    dataset_mib: int = DATASET_MIB
    seed: int = SEED
    warmup_seconds: int = WARMUP_SECONDS
    runtime_seconds: int = RUNTIME_SECONDS

    @property
    def case(self) -> str:
        write_percent = 100 - self.read_percent
        return (
            f"mixed-{self.device}-{self.mode}-r{self.read_percent}w{write_percent}-"
            f"sync{self.sync_percent}-r{self.repeat}"
        )

    @property
    def file_bytes(self) -> int:
        return self.file_mib * 1024 * 1024

    @property
    def virtqueues(self) -> int:
        return 8 if self.device == "nvme" else 1

    @property
    def disk_format(self) -> str:
        return {"ext2": "ext2", "ext4": "journal", "rnvpce": "nojournal"}[self.mode]

    @property
    def dram_cache_mib(self) -> int:
        return 128 if self.mode == "rnvpce" else 256

    @property
    def nv_data_cache_mib(self) -> int:
        return 128 if self.mode == "rnvpce" else 0

    @property
    def cache_control(self) -> str:
        return "uncontrolled" if self.mode == "ext2" else "strict"


def matrix() -> list[Point]:
    return [
        Point(device, mode, read_percent, sync_percent, repeat)
        for repeat in REPEATS
        for device in DEVICES
        for mode in MODES
        for read_percent in READ_PERCENTAGES
        for sync_percent in SYNC_PERCENTAGES
    ]


def point_by_case(case: str) -> Point:
    try:
        return next(point for point in matrix() if point.case == case)
    except StopIteration as error:
        raise ValueError(f"unknown case: {case}") from error


def print_point(point: Point) -> None:
    fields = (
        point.case,
        point.device,
        point.mode,
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
        point.disk_format,
        point.dram_cache_mib,
        point.nv_data_cache_mib,
        point.cache_control,
    )
    print("\t".join(map(str, fields)))


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
