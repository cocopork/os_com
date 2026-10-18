#!/usr/bin/env python3
"""Parse clean-remount timing markers into raw and six-point summaries."""

from __future__ import annotations

import csv
import math
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path


MARKER = re.compile(
    r"=== REMOUNT_RESULT "
    r"profile=(?P<profile>\S+) "
    r"data_mb=(?P<data_mb>\d+) "
    r"repeat=(?P<repeat>\d+) "
    r"remount_ns=(?P<remount_ns>\d+) "
    r"file_size=(?P<file_size>\d+) "
    r"valid=(?P<valid>[01])"
    r"(?: verify_ok=(?P<verify_ok>[01]))? ==="
)
PROFILE_BEGIN = re.compile(r"=== REMOUNT_PROFILE_BEGIN .* ===")
MOUNT_PROFILE = re.compile(
    r"\[MOUNT_PROFILE\] "
    r"mode=(?P<mode>\S+) "
    r"total_us=(?P<total_us>\d+) "
    r"super_options_us=(?P<super_options_us>\d+) "
    r"block_groups_us=(?P<block_groups_us>\d+) "
    r"construct_us=(?P<construct_us>\d+) "
    r"journal_us=(?P<journal_us>\d+) "
    r"writeback_worker_us=(?P<writeback_worker_us>\d+) "
    r"rnvpc_worker_us=(?P<rnvpc_worker_us>\d+)"
)
PROFILE_PHASES = (
    "total",
    "super_options",
    "block_groups",
    "construct",
    "journal",
    "writeback_worker",
    "rnvpc_worker",
)
RAW_FIELDS = (
    "profile",
    "data_mb",
    "repeat",
    "remount_ns",
    "remount_ms",
    "file_size",
    "valid",
    "verify_ok",
    "mount_mode",
    *(f"mount_{phase}_us" for phase in PROFILE_PHASES),
)
SUMMARY_FIELDS = (
    "profile",
    "data_mb",
    "repeats",
    "valid_repeats",
    "median_ms",
    "p95_ms",
    "min_ms",
    "max_ms",
    *(f"mount_{phase}_median_us" for phase in PROFILE_PHASES),
)


def parse(log_path: Path) -> list[dict[str, object]]:
    rows = []
    awaiting_profile = False
    mount_profile: dict[str, str] | None = None
    for line in log_path.read_text(encoding="utf-8", errors="replace").splitlines():
        if PROFILE_BEGIN.search(line):
            awaiting_profile = True
            mount_profile = None
            continue
        profile_match = MOUNT_PROFILE.search(line)
        if awaiting_profile and profile_match is not None:
            mount_profile = profile_match.groupdict()
            awaiting_profile = False
            continue
        match = MARKER.search(line)
        if match is None:
            continue
        values = match.groupdict()
        remount_ns = int(values["remount_ns"])
        row: dict[str, object] = {
            "profile": values["profile"],
            "data_mb": int(values["data_mb"]),
            "repeat": int(values["repeat"]),
            "remount_ns": remount_ns,
            "remount_ms": remount_ns / 1_000_000,
            "file_size": int(values["file_size"]),
            "valid": int(values["valid"]),
            # Older clean-remount logs predate content verification.
            "verify_ok": int(values["verify_ok"] or 1),
            "mount_mode": mount_profile["mode"] if mount_profile else "",
        }
        for phase in PROFILE_PHASES:
            row[f"mount_{phase}_us"] = (
                int(mount_profile[f"{phase}_us"]) if mount_profile else ""
            )
        rows.append(row)
        mount_profile = None
        awaiting_profile = False
    rows.sort(key=lambda row: (str(row["profile"]), int(row["data_mb"]), int(row["repeat"])))
    return rows


def nearest_rank(values: list[float], percentile: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(percentile * len(ordered)) - 1)]


def summarize(rows: list[dict[str, object]]) -> list[dict[str, object]]:
    groups: dict[tuple[str, int], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        groups[(str(row["profile"]), int(row["data_mb"]))].append(row)

    summary = []
    for (profile, data_mb), samples in sorted(groups.items()):
        valid = [
            float(row["remount_ms"])
            for row in samples
            if int(row["valid"]) == 1 and int(row["verify_ok"]) == 1
        ]
        summary.append(
            {
                "profile": profile,
                "data_mb": data_mb,
                "repeats": len(samples),
                "valid_repeats": len(valid),
                "median_ms": f"{statistics.median(valid):.3f}" if valid else "",
                "p95_ms": f"{nearest_rank(valid, 0.95):.3f}" if valid else "",
                "min_ms": f"{min(valid):.3f}" if valid else "",
                "max_ms": f"{max(valid):.3f}" if valid else "",
                **{
                    f"mount_{phase}_median_us": (
                        f"{statistics.median(phase_values):.3f}" if phase_values else ""
                    )
                    for phase in PROFILE_PHASES
                    for phase_values in [[
                        float(row[f"mount_{phase}_us"])
                        for row in samples
                        if int(row["valid"]) == 1
                        and int(row["verify_ok"]) == 1
                        and row[f"mount_{phase}_us"] != ""
                    ]]
                },
            }
        )
    return summary


def write_csv(path: Path, fields: tuple[str, ...], rows: list[dict[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def main() -> int:
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} LOG RAW_CSV SUMMARY_CSV", file=sys.stderr)
        return 2
    rows = parse(Path(sys.argv[1]))
    if not rows:
        print("error: no REMOUNT_RESULT markers found", file=sys.stderr)
        return 1
    write_csv(Path(sys.argv[2]), RAW_FIELDS, rows)
    write_csv(Path(sys.argv[3]), SUMMARY_FIELDS, summarize(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
