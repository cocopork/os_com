#!/usr/bin/env python3
"""Parses the single fio JSON payload from one host-path probe guest log."""

import json
import re
import sys


PAYLOAD = re.compile(
    r"VIRTIO_DIRECT_FIO_START repeat=(\d+)\r?\n(.*?)\r?\n"
    r"VIRTIO_DIRECT_FIO_END repeat=\1",
    re.DOTALL,
)
PROFILE_LINE = re.compile(r"\[VIRTIO_PROFILE\]\s+([^\r\n]+)")
MODULE_LINE = re.compile(r"\[(RAW_BLOCK_PROFILE|BLOCK_MODULE_PROFILE)\]\s+([^\r\n]+)")
ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")


def parse_log(text: str, case: str, expected_bytes: int) -> list[dict]:
    rows = []
    for repeat_text, payload_text in PAYLOAD.findall(text):
        payload_text = payload_text.replace("\r", "")
        payload_text = "\n".join(
            line for line in payload_text.splitlines()
            if "[CPU-ISOLATION]" not in line
        )
        payload = json.loads(payload_text[payload_text.index("{"):])
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
    if [row["repeat"] for row in rows] != [1]:
        raise ValueError("expected exactly repetition 1")
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


def parse_module_profile(text: str, case: str) -> dict[str, dict]:
    latest = {}
    for kind, payload in MODULE_LINE.findall(ANSI_ESCAPE.sub("", text)):
        values = {
            key: int(value)
            for key, value in re.findall(r"([a-z0-9_]+)=(\d+)", payload)
        }
        if "samples" not in values:
            continue
        if kind not in latest or values["samples"] > latest[kind]["samples"]:
            latest[kind] = values
    required = {"RAW_BLOCK_PROFILE", "BLOCK_MODULE_PROFILE"}
    if set(latest) != required:
        raise ValueError("incomplete module profile snapshots")

    raw = {"case": case, **latest["RAW_BLOCK_PROFILE"]}
    block = {"case": case, **latest["BLOCK_MODULE_PROFILE"]}
    for row in (raw, block):
        samples = row["samples"]
        tsc_hz = row["tsc_hz"]
        for key, value in list(row.items()):
            if key.endswith("_ticks") and key not in ("virtio_e2e_ticks", "resume_ticks"):
                row[f"{key.removesuffix('_ticks')}_us_per_sample"] = (
                    value * 1_000_000 / tsc_hz / samples
                )
    block["virtio_e2e_us_per_sample"] = (
        block["virtio_e2e_ticks"] * 1_000_000 /
        block["tsc_hz"] / block["virtio_samples"]
    )
    block["completion_to_resume_us_per_sample"] = (
        block["resume_ticks"] * 1_000_000 /
        block["tsc_hz"] / block["resume_samples"]
    )
    return {"raw": raw, "block": block}


def main() -> None:
    if len(sys.argv) == 4 and sys.argv[1] == "profile":
        rows = parse_profile(open(sys.argv[2]).read(), sys.argv[3])
    elif len(sys.argv) == 4 and sys.argv[1] == "module":
        rows = parse_module_profile(open(sys.argv[2]).read(), sys.argv[3])
    elif len(sys.argv) == 4:
        rows = parse_log(open(sys.argv[1]).read(), sys.argv[2], int(sys.argv[3]))
    else:
        raise SystemExit(
            f"usage: {sys.argv[0]} LOG CASE EXPECTED_BYTES | "
            "profile|module LOG CASE"
        )
    json.dump(rows, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
