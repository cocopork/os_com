#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Parses repeated raw-device fio results from one guest run."""

import json
from pathlib import Path
import re
import sys


MIB = 1024 * 1024


def parse_point(point_dir: Path, point) -> list[dict]:
    raw = (point_dir / "qemu.log").read_bytes().decode(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)
    completion = re.search(
        rf"^RAW_DEVICE_COMPLETE case={re.escape(point.case)} (.*)$", raw, re.M
    )
    if not completion:
        raise ValueError(f"{point.case}: missing completion marker")
    fields = dict(re.findall(r"(\w+)=([^\s]+)", completion.group(1)))
    if fields.get("fio_status") != "0":
        raise ValueError(f"{point.case}: fio_status is not zero")
    if fields.get("completed_repeats") != str(point.repeats):
        raise ValueError(f"{point.case}: completed_repeats does not match")

    operation = "read" if point.fio_rw == "randread" else "write"
    rows = []
    for repeat in range(1, point.repeats + 1):
        begin = f"RAW_DEVICE_FIO_START case={point.case} repeat={repeat}\n"
        end = f"RAW_DEVICE_FIO_END case={point.case} repeat={repeat}"
        if begin not in raw or end not in raw:
            raise ValueError(f"{point.case}: missing fio markers for repeat {repeat}")
        payload = raw.split(begin, 1)[1].split(end, 1)[0]
        fio, _ = json.JSONDecoder().raw_decode(payload[payload.index("{") :])
        jobs = fio.get("jobs", [])
        if len(jobs) != 1 or jobs[0].get("error") != 0:
            raise ValueError(f"{point.case}: fio failed in repeat {repeat}")
        measured = jobs[0][operation]
        if int(measured.get("io_bytes", -1)) != point.expected_bytes:
            raise ValueError(f"{point.case}: fio byte count is incomplete in repeat {repeat}")
        rows.append(
            {
                "case": point.case,
                "device": point.device,
                "system": point.system,
                "workload": point.workload,
                "axis": point.axis,
                "bs_kib": point.bs_kib,
                "jobs": point.jobs,
                "repeat": repeat,
                "actual_mib": point.actual_mib,
                "virtqueues": 8 if point.device == "nvme" else 1,
                "fio_mib_s": int(measured["bw_bytes"]) / MIB,
                "iops": float(measured["iops"]),
                "mean_latency_us": float(measured["lat_ns"]["mean"]) / 1000,
                "p99_latency_us": float(
                    measured["clat_ns"].get("percentile", {}).get("99.000000", 0)
                )
                / 1000,
            }
        )
    return rows


def main() -> None:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import matrix

    point_dir = Path(sys.argv[1])
    point = matrix.point_by_case(sys.argv[2])
    print(json.dumps(parse_point(point_dir, point), indent=2))


if __name__ == "__main__":
    main()
