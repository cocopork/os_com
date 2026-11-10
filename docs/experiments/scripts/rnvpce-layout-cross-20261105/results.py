#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import json
from pathlib import Path
import re
import sys


def parse(point: Path) -> dict:
    raw = (point / "qemu.log").read_bytes().decode(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)
    match = re.search(r"^LAYOUT_CROSS_COMPLETE (.*)$", raw, re.M)
    if not match:
        raise ValueError("missing guest completion marker")
    fields = dict(re.findall(r"(\w+)=([^\s]+)", match.group(1)))
    for name in ("fio_status", "sync_status", "umount_status"):
        if fields.get(name) != "0":
            raise ValueError(f"{name} is not zero")
    case = fields["case"]
    payload = raw.split(f"LAYOUT_CROSS_FIO_START {case}\n", 1)[1].split(
        f"LAYOUT_CROSS_FIO_END {case}", 1
    )[0]
    fio, _ = json.JSONDecoder().raw_decode(payload[payload.index("{"):])
    if len(fio["jobs"]) != 1:
        raise ValueError("grouped fio result must contain one aggregate job")
    job = fio["jobs"][0]
    write = job["write"]
    expected_bytes = int(fields["expected_bytes"])
    if job["error"] != 0 or write["io_bytes"] != expected_bytes:
        raise ValueError("fio failed or wrote an unexpected byte count")
    fio_elapsed = (int(fields["fio_end_ns"]) - int(fields["start_ns"])) / 1e9
    sync_elapsed = (int(fields["sync_end_ns"]) - int(fields["start_ns"])) / 1e9
    if fio_elapsed <= 0 or sync_elapsed <= 0:
        raise ValueError("invalid elapsed interval")
    total_mib = expected_bytes / 1024**2
    return {
        "case": case,
        "layout": fields["layout"],
        "jobs": int(fields["jobs"]),
        "expected_bytes": expected_bytes,
        "fio_mib_s": write["bw_bytes"] / 1024**2,
        "wall_fio_mib_s": total_mib / fio_elapsed,
        "including_sync_mib_s": total_mib / sync_elapsed,
        "iops": write["iops"],
        "mean_latency_us": write["lat_ns"]["mean"] / 1000,
        "p99_latency_us": write["clat_ns"].get("percentile", {}).get("99.000000", 0) / 1000,
        "hctx": json.loads((point / "hctx-counts.json").read_text()),
        "mapping": json.loads((point / "mapping.json").read_text()),
    }


if __name__ == "__main__":
    print(json.dumps(parse(Path(sys.argv[1])), indent=2))
