#!/usr/bin/env python3
"""Parse rNVPCe pure-write and mixed-read/write fio results."""

from __future__ import annotations

import argparse
import csv
import json
import re
from pathlib import Path


MARKER_RE = re.compile(
    r"=== FIO_RESULT: (?P<device>sata|nvme)_"
    r"(?P<fs>rnvpc_extend)_"
    r"(?P<workload>randwrite-4k|randrw-r(?:25|50|75)-4k)_"
    r"(?P<mode>orderless)_nj(?P<njobs>\d+) ==="
)
DRAIN_RE = re.compile(r"=== DRAIN: busy_ns=(\d+) drain_ns=(\d+) ===")


def _mib(value: int) -> float:
    return value / (1024 * 1024)


def parse_log(path: Path) -> list[dict[str, object]]:
    text = path.read_text(errors="replace")
    matches = list(MARKER_RE.finditer(text))
    rows: list[dict[str, object]] = []
    decoder = json.JSONDecoder()
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        segment = text[match.end():end]
        json_start = segment.find("{")
        drain = DRAIN_RE.search(segment)
        if json_start < 0 or not drain:
            continue
        payload, _ = decoder.raw_decode(segment[json_start:])
        if len(payload.get("jobs", [])) != 1:
            raise ValueError(f"expected group-reported fio output for {match.group(0)}")
        job = payload["jobs"][0]
        read_bytes = int(job["read"]["io_bytes"])
        write_bytes = int(job["write"]["io_bytes"])
        total_bytes = read_bytes + write_bytes
        busy_ns = int(drain.group(1))
        drain_ns = int(drain.group(2))
        if total_bytes <= 0 or busy_ns <= 0 or drain_ns <= 0:
            continue

        def bandwidth(byte_count: int, elapsed_ns: int) -> float:
            return _mib(byte_count) / (elapsed_ns / 1_000_000_000)

        rows.append({
            **match.groupdict(),
            "njobs": int(match.group("njobs")),
            "read_mib": _mib(read_bytes),
            "write_mib": _mib(write_bytes),
            "total_mib": _mib(total_bytes),
            "read_ratio_pct": read_bytes / total_bytes * 100.0,
            "busy_ms": busy_ns / 1_000_000,
            "drain_ms": drain_ns / 1_000_000,
            "fio_read_bw_mib_s": int(job["read"]["bw_bytes"]) / (1024 * 1024),
            "fio_write_bw_mib_s": int(job["write"]["bw_bytes"]) / (1024 * 1024),
            "busy_read_bw_mib_s": bandwidth(read_bytes, busy_ns),
            "busy_write_bw_mib_s": bandwidth(write_bytes, busy_ns),
            "busy_total_bw_mib_s": bandwidth(total_bytes, busy_ns),
            "drain_read_bw_mib_s": bandwidth(read_bytes, drain_ns),
            "drain_write_bw_mib_s": bandwidth(write_bytes, drain_ns),
            "drain_total_bw_mib_s": bandwidth(total_bytes, drain_ns),
            "read_iops": float(job["read"]["iops"]),
            "write_iops": float(job["write"]["iops"]),
        })
    return rows


def write_csv(path: Path, rows: list[dict[str, object]]) -> None:
    if not rows:
        raise ValueError("no complete mixed workload results found")
    fields = list(rows[0])
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        for row in rows:
            writer.writerow({
                key: f"{value:.4f}" if isinstance(value, float) else value
                for key, value in row.items()
            })


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    rows = parse_log(args.log)
    write_csv(args.output, rows)
    for row in rows:
        print(
            f"{row['workload']:16s} read={row['read_ratio_pct']:5.1f}% "
            f"busy={row['busy_total_bw_mib_s']:8.2f} MiB/s "
            f"drain={row['drain_total_bw_mib_s']:8.2f} MiB/s "
            f"(read={row['drain_read_bw_mib_s']:7.2f}, "
            f"write={row['drain_write_bw_mib_s']:7.2f})"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
