#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Extract rNVPCe foreground latency phases from serial logs."""

import csv
import re
import sys
from pathlib import Path


METRIC = re.compile(
    r"\[LATENCY\] (fg_[a-z_]+|rnvpce_(?:write_inner_wait|publication_wait|publication_hold)) "
    r"count=(\d+) sum_ns=(\d+) max_ns=(\d+)"
)
PHASES = (
    "rnvpce_write_inner_wait", "rnvpce_publication_wait", "rnvpce_publication_hold",
    "fg_body", "fg_inode_read", "fg_lookup", "fg_allocate", "fg_data",
    "fg_data_init", "fg_copy", "fg_data_register", "fg_reserve", "fg_extent",
    "fg_repair_image", "fg_repair_mark", "fg_repair_apply", "fg_repair_verify",
    "fg_repair_cleanup", "fg_reverse_refresh", "fg_inode_publish", "fg_enqueue",
)


def main():
    root = Path(sys.argv[1]).resolve()
    rows = []
    for path in sorted(root.glob("rnvpce-write*-j*-2048m.log")):
        values = {name: (int(count), int(total), int(maximum))
                  for name, count, total, maximum in METRIC.findall(path.read_text(errors="replace"))}
        missing = [name for name in PHASES if name not in values]
        if missing:
            raise SystemExit(f"{path.name}: missing metrics: {', '.join(missing)}")
        match = re.fullmatch(r"rnvpce-write(4k|16k|64k|128k)-j(\d+)-2048m", path.stem)
        if not match:
            continue
        bs, jobs = match.groups()
        body_count, _, _ = values["fg_body"]
        for phase in PHASES:
            count, total, maximum = values[phase]
            rows.append({
                "case": path.stem, "bs": bs, "jobs": jobs, "phase": phase,
                "count": count, "sum_ns": total, "max_ns": maximum,
                "us_per_write": total / body_count / 1000,
                "us_per_event": total / count / 1000 if count else 0,
            })
    if not rows:
        raise SystemExit("no foreground latency logs found")
    with (root / "foreground-phases.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    print(f"cases={len(rows) // len(PHASES)} rows={len(rows)}")


if __name__ == "__main__":
    main()
