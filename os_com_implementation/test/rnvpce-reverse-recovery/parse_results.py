#!/usr/bin/env python3

import csv
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path


BEGIN_RE = re.compile(
    r"RNVPC_RECOVERY_RUN_BEGIN target_files=(\d+) created_files=(\d+) "
    r"data_bytes=(\d+) fill_status=(\d+) repeat=(\d+)"
)
PROFILE_RE = re.compile(r"\[RNVPC_RECOVERY_PROFILE\] (.*)")
PAIR_RE = re.compile(r"([a-z_]+)=(\d+)")


def parse(log_path: Path):
    rows = []
    current = None
    for line in log_path.read_text(errors="replace").splitlines():
        if match := BEGIN_RE.search(line):
            current = {
                "target_files": int(match.group(1)),
                "created_files": int(match.group(2)),
                "data_bytes": int(match.group(3)),
                "fill_status": int(match.group(4)),
                "repeat": int(match.group(5)),
            }
            continue
        if current is not None and (match := PROFILE_RE.search(line)):
            values = {key: int(value) for key, value in PAIR_RE.findall(match.group(1))}
            rows.append({**current, **values})
            current = None
    return rows


def write_csv(path: Path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    fields = list(rows[0]) if rows else []
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        if fields:
            writer.writeheader()
            writer.writerows(rows)


def summarize(rows):
    grouped = defaultdict(list)
    for row in rows:
        grouped[row["target_files"]].append(row)
    summary = []
    timing_fields = [key for key in rows[0] if key.endswith("_ns")] if rows else []
    for target, group in sorted(grouped.items()):
        item = {
            "target_files": target,
            "created_files": max(row["created_files"] for row in group),
            "data_bytes": max(row["data_bytes"] for row in group),
            "samples": len(group),
            "fill_status": max(row["fill_status"] for row in group),
            "inode_roots": int(statistics.median(row["inode_roots"] for row in group)),
            "extent_nodes": int(statistics.median(row["extent_nodes"] for row in group)),
            "extent_entries": int(statistics.median(row["extent_entries"] for row in group)),
            "reverse_entries": int(statistics.median(row["reverse_entries"] for row in group)),
            "pool_total": int(statistics.median(row["pool_total"] for row in group)),
            "pool_free": int(statistics.median(row["pool_free"] for row in group)),
            "pool_meta": int(statistics.median(row["pool_meta"] for row in group)),
            "pool_data": int(statistics.median(row["pool_data"] for row in group)),
        }
        for field in timing_fields:
            values = [row[field] for row in group]
            item[f"{field[:-3]}_median_ms"] = round(statistics.median(values) / 1_000_000, 3)
            item[f"{field[:-3]}_min_ms"] = round(min(values) / 1_000_000, 3)
            item[f"{field[:-3]}_max_ms"] = round(max(values) / 1_000_000, 3)
        summary.append(item)
    return summary


def main():
    if len(sys.argv) != 4:
        raise SystemExit("usage: parse_results.py LOG RAW_CSV SUMMARY_CSV")
    rows = parse(Path(sys.argv[1]))
    if not rows:
        raise SystemExit("no RNVPC recovery profile records found")
    write_csv(Path(sys.argv[2]), rows)
    write_csv(Path(sys.argv[3]), summarize(rows))


if __name__ == "__main__":
    main()
