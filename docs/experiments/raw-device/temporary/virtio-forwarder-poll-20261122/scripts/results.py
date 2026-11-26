#!/usr/bin/env python3
"""Parses the three fio JSON payloads from one experiment guest log."""

import json
import re
import sys


PAYLOAD = re.compile(
    r"VIRTIO_POLL_FIO_START repeat=(\d+)\r?\n(.*?)\r?\n"
    r"VIRTIO_POLL_FIO_END repeat=\1",
    re.DOTALL,
)


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


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(f"usage: {sys.argv[0]} LOG CASE EXPECTED_BYTES")
    rows = parse_log(open(sys.argv[1]).read(), sys.argv[2], int(sys.argv[3]))
    json.dump(rows, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
