#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Extract the final rTIO metric snapshot and compare it with the 010 profile."""

import json
import re
from pathlib import Path


RUNS = Path(
    "/home/zn/asterinas-build-env/orderless-migration-20261019/profile-default/runs"
)
BASELINE = Path(
    "/home/zn/asterinas-build-env/nvpage-batch-20261018/profile/diagnostics.json"
)
OUTPUT = Path(
    "/home/zn/asterinas-build-env/orderless-migration-20261019/profile-default.json"
)
PAGES = 524_288

PHASE_PATTERN = re.compile(
    r"\[NVPAGE-BATCH\] phase=(\w+) calls=(\d+) sum_ns=(\d+) max_ns=(\d+)"
)
EVENT_PATTERN = re.compile(r"\[NVPAGE-BATCH\] event=(\w+) count=(\d+)")


def snapshots(pattern, text):
    matches = pattern.findall(text)
    width = 7
    assert matches and len(matches) % width == 0, (pattern.pattern, len(matches))
    return matches[-width:]


def parse_current(log):
    text = log.read_text(errors="replace")
    phases = {}
    for name, calls, sum_ns, max_ns in snapshots(PHASE_PATTERN, text):
        calls = int(calls)
        sum_ns = int(sum_ns)
        phases[name] = {
            "calls": calls,
            "sum_ns": sum_ns,
            "max_ns": int(max_ns),
            "avg_ns_per_call": sum_ns / calls if calls else 0,
            "avg_ns_per_page": sum_ns / PAGES,
        }
    events = {}
    for name, count in snapshots(EVENT_PATTERN, text):
        count = int(count)
        events[name] = {"count": count, "per_page": count / PAGES}
    match = re.match(r"rnvpce-(randwrite|write)-j(1|16)-2048m", log.stem)
    assert match, log
    return {
        "case": log.stem,
        "rw": match.group(1),
        "jobs": int(match.group(2)),
        "pages": PAGES,
        "phases": phases,
        "events": events,
    }


def main():
    current = [parse_current(log) for log in sorted(RUNS.glob("rnvpce-*-2048m.log"))]
    baseline = json.loads(BASELINE.read_text())
    baseline = {
        (row["rw"], row["jobs"]): row
        for row in baseline
        if row["version"] == "after"
    }
    for row in current:
        old = baseline[(row["rw"], row["jobs"])]
        row["baseline_010"] = {"phases": old["phases"], "events": old["events"]}
        row["phase_sum_change_pct"] = {
            name: (
                (phase["sum_ns"] / old["phases"][name]["sum_ns"] - 1) * 100
                if old["phases"][name]["sum_ns"]
                else None
            )
            for name, phase in row["phases"].items()
        }
        row["event_change_pct"] = {
            name: (
                (event["count"] / old["events"][name] - 1) * 100
                if old["events"][name]
                else None
            )
            for name, event in row["events"].items()
        }
    OUTPUT.write_text(json.dumps(current, indent=2) + "\n")
    print(OUTPUT)


if __name__ == "__main__":
    main()
