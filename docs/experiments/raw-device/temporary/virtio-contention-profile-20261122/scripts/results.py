#!/usr/bin/env python3
"""Parses the three fio JSON payloads from one experiment guest log."""

import json
import re
import sys


PAYLOAD = re.compile(
    r"VIRTIO_DIRECT_FIO_START repeat=(\d+)\r?\n(.*?)\r?\n"
    r"VIRTIO_DIRECT_FIO_END repeat=\1",
    re.DOTALL,
)
PROFILE_LINE = re.compile(r"\[VIRTIO_PROFILE\]\s+([^\r\n]+)")
ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")


def parse_log(text: str, case: str, expected_bytes: int) -> list[dict]:
    rows = []
    for repeat_text, payload_text in PAYLOAD.findall(text):
        payload = json.loads(payload_text.replace("\r", ""))
        write = payload["jobs"][0]["write"]
        if write["io_bytes"] != expected_bytes:
            raise ValueError(
                f"short I/O: {write['io_bytes']} != {expected_bytes}"
            )
        latency = write["clat_ns"]
        rows.append({
            "case": case,
            "repeat": int(repeat_text),
            "io_bytes": write["io_bytes"],
            "fio_mib_s": write["bw_bytes"] / (1024 * 1024),
            "iops": write["iops"],
            "mean_latency_us": latency["mean"] / 1000,
            "p99_latency_us": latency["percentile"]["99.000000"] / 1000,
        })
    if [row["repeat"] for row in rows] != [1, 2, 3]:
        raise ValueError("expected exactly repetitions 1, 2, and 3")
    return rows


def parse_profile(text: str, case: str) -> list[dict]:
    latest = {}
    for payload in PROFILE_LINE.findall(ANSI_ESCAPE.sub("", text)):
        values = {}
        for item in payload.split():
            key, value = item.split("=", 1)
            values[key] = int(value)
        queue = values["queue"]
        if queue not in latest or values["completions"] > latest[queue]["completions"]:
            latest[queue] = values
    if not latest:
        raise ValueError("no virtio profile snapshots")

    rows = []
    for queue, values in sorted(latest.items()):
        row = {"case": case, **values}
        tsc_hz = values["tsc_hz"]
        for key, value in values.items():
            if not key.endswith("_ticks"):
                continue
            if key.startswith("submit_"):
                samples = values["submit_samples"]
            elif key.startswith("completion_"):
                samples = values["completion_samples"]
            elif key == "descriptor_wait_ticks":
                samples = values["descriptor_wait_count"]
            else:
                continue
            row[f"{key.removesuffix('_ticks')}_us_per_sample"] = (
                value * 1_000_000 / tsc_hz / samples if samples else 0.0
            )
        rows.append(row)
    return rows


def main() -> None:
    if len(sys.argv) == 4 and sys.argv[1] == "profile":
        rows = parse_profile(open(sys.argv[2]).read(), sys.argv[3])
    elif len(sys.argv) == 4:
        rows = parse_log(open(sys.argv[1]).read(), sys.argv[2], int(sys.argv[3]))
    else:
        raise SystemExit(
            f"usage: {sys.argv[0]} LOG CASE EXPECTED_BYTES | profile LOG CASE"
        )
    json.dump(rows, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
