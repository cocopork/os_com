#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Validates and normalizes one mixed-sync benchmark result."""

import argparse
import json
import math
from pathlib import Path
import re

import matrix


BLOCK_SIZE = 4096


def require_integer(payload: dict, field: str, case: str, *, minimum: int = 0) -> int:
    value = payload.get(field)
    if type(value) is not int or value < minimum:
        raise ValueError(f"{case}: invalid {field}")
    return value


def require_finite(payload: dict, field: str, case: str, *, positive: bool = False) -> float:
    value = payload.get(field)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{case}: invalid {field}")
    result = float(value)
    if not math.isfinite(result) or (result <= 0 if positive else result < 0):
        raise ValueError(f"{case}: invalid {field}")
    return result


def fields_from_line(line: str) -> dict[str, str]:
    return dict(re.findall(r"([a-zA-Z_][a-zA-Z0-9_]*)=([^\s]+)", line))


def ratio_is_plausible(count: int, total: int, expected_percent: int) -> bool:
    if total <= 0:
        return expected_percent == 0 and count == 0
    if expected_percent in (0, 100):
        return count * 100 == expected_percent * total
    probability = expected_percent / 100
    tolerance = max(1 / total, 6 * math.sqrt(probability * (1 - probability) / total))
    return abs(count / total - probability) <= tolerance


def parse_point(point_dir: Path, point: matrix.Point) -> dict:
    raw = (point_dir / "qemu.log").read_text(errors="replace").replace("\r", "")
    raw = re.sub(r"\x1b\[[0-9;]*m", "", raw)

    completion_match = re.search(
        rf"^MIXED_SYNC_COMPLETE case={re.escape(point.case)} (.*)$", raw, re.M
    )
    if completion_match is None:
        raise ValueError(f"{point.case}: missing completion marker")
    completion = fields_from_line(completion_match.group(1))
    for field in ("benchmark_status", "sync_status", "umount_status"):
        if completion.get(field) != "0":
            raise ValueError(f"{point.case}: {field} is not zero")

    config_match = re.search(
        rf"^MIXED_SYNC_CONFIG case={re.escape(point.case)} (.*)$", raw, re.M
    )
    if config_match is None:
        raise ValueError(f"{point.case}: missing runtime configuration")
    runtime_config = fields_from_line(config_match.group(1))

    begin = f"MIXED_SYNC_JSON_BEGIN case={point.case}\n"
    end = f"MIXED_SYNC_JSON_END case={point.case}"
    if begin not in raw or end not in raw:
        raise ValueError(f"{point.case}: missing benchmark JSON markers")
    payload_text = raw.split(begin, 1)[1].split(end, 1)[0].strip()
    try:
        payload = json.loads(payload_text)
    except json.JSONDecodeError as error:
        raise ValueError(f"{point.case}: invalid benchmark JSON") from error

    expected_payload = {
        "threads": point.threads,
        "files_opened": point.threads,
        "file_bytes": point.file_bytes,
        "read_percent": point.read_percent,
        "sync_percent": point.sync_percent,
        "seed": point.seed,
        "warmup_seconds": point.warmup_seconds,
        "runtime_seconds": point.runtime_seconds,
    }
    for field, expected in expected_payload.items():
        if payload.get(field) != expected:
            raise ValueError(
                f"{point.case}: {field} mismatch: {payload.get(field)!r} != {expected!r}"
            )

    operations = require_integer(payload, "operations", point.case, minimum=1)
    reads = require_integer(payload, "reads", point.case)
    writes = require_integer(payload, "writes", point.case)
    fsyncs = require_integer(payload, "fsyncs", point.case)
    read_bytes = require_integer(payload, "read_bytes", point.case)
    write_bytes = require_integer(payload, "write_bytes", point.case)
    latency_overflow_count = require_integer(
        payload, "latency_overflow_count", point.case
    )
    if reads + writes != operations:
        raise ValueError(f"{point.case}: operation counts are inconsistent")
    if fsyncs > writes:
        raise ValueError(f"{point.case}: sync count exceeds writes")
    if read_bytes != reads * BLOCK_SIZE:
        raise ValueError(f"{point.case}: read byte count is inconsistent")
    if write_bytes != writes * BLOCK_SIZE:
        raise ValueError(f"{point.case}: write byte count is inconsistent")
    if not ratio_is_plausible(reads, operations, point.read_percent):
        raise ValueError(f"{point.case}: observed read ratio is implausible")
    if not ratio_is_plausible(fsyncs, writes, point.sync_percent):
        raise ValueError(f"{point.case}: observed sync ratio is implausible")

    prewrite_matches = re.findall(
        r"^ACTION PREWRITE_FILE thread=(\d+) path=\S+ bytes=(\d+)$", raw, re.M
    )
    prewrite = {int(thread): int(byte_count) for thread, byte_count in prewrite_matches}
    if set(prewrite) != set(range(point.threads)) or any(
        byte_count != point.file_bytes for byte_count in prewrite.values()
    ):
        raise ValueError(f"{point.case}: incomplete or invalid prewrite evidence")

    try:
        e2fsck_status = int((point_dir / "e2fsck.status").read_text().strip())
    except (FileNotFoundError, ValueError) as error:
        raise ValueError(f"{point.case}: missing e2fsck status") from error
    if e2fsck_status != 0:
        raise ValueError(f"{point.case}: e2fsck failed with {e2fsck_status}")
    if "rtio_cpu_trace=on" in raw or "rtio_cpu_trace=off" not in raw:
        raise ValueError(f"{point.case}: tracing was not demonstrably disabled")

    expected_runtime_config = {
        "device": point.device,
        "mode": point.mode,
        "read_percent": str(point.read_percent),
        "sync_percent": str(point.sync_percent),
        "repeat": str(point.repeat),
        "threads": str(point.threads),
        "dataset_mib": str(point.dataset_mib),
        "seed": str(point.seed),
        "cache_control": point.cache_control,
        "configured_dram_cache_mib": str(point.dram_cache_mib),
        "configured_nv_data_cache_mib": str(point.nv_data_cache_mib),
        "configured_total_data_cache_mib": "256",
    }
    for field, expected in expected_runtime_config.items():
        if runtime_config.get(field) != expected:
            raise ValueError(f"{point.case}: runtime {field} mismatch")

    elapsed_seconds = require_finite(
        payload, "elapsed_seconds", point.case, positive=True
    )
    operations_per_second = require_finite(
        payload, "operations_per_second", point.case, positive=True
    )
    mib_per_second = require_finite(
        payload, "mib_per_second", point.case, positive=True
    )
    expected_ops = operations / elapsed_seconds
    expected_mib = (read_bytes + write_bytes) / (1024 * 1024) / elapsed_seconds
    if not math.isclose(operations_per_second, expected_ops, rel_tol=1e-5, abs_tol=1e-6):
        raise ValueError(f"{point.case}: operations_per_second is inconsistent")
    if not math.isclose(mib_per_second, expected_mib, rel_tol=1e-5, abs_tol=1e-6):
        raise ValueError(f"{point.case}: mib_per_second is inconsistent")

    latency_fields = (
        "mean_latency_us",
        "p50_latency_us",
        "p95_latency_us",
        "p99_latency_us",
        "p999_latency_us",
    )
    latencies = {
        field: require_finite(payload, field, point.case) for field in latency_fields
    }
    if not (
        latencies["p50_latency_us"] <= latencies["p95_latency_us"]
        <= latencies["p99_latency_us"] <= latencies["p999_latency_us"]
    ):
        raise ValueError(f"{point.case}: latency quantiles are inconsistent")

    return {
        "case": point.case,
        "device": point.device,
        "mode": point.mode,
        "read_percent": point.read_percent,
        "write_percent": 100 - point.read_percent,
        "sync_percent": point.sync_percent,
        "repeat": point.repeat,
        "threads": point.threads,
        "file_mib": point.file_mib,
        "dataset_mib": point.dataset_mib,
        "configured_dram_cache_mib": point.dram_cache_mib,
        "configured_nv_data_cache_mib": point.nv_data_cache_mib,
        "configured_total_data_cache_mib": 256,
        "cache_control": point.cache_control,
        "prewrite_bytes": sum(prewrite.values()),
        "operations": operations,
        "reads": reads,
        "writes": writes,
        "fsyncs": fsyncs,
        "actual_read_percent": reads * 100 / operations if operations else 0.0,
        "actual_sync_percent": fsyncs * 100 / writes if writes else 0.0,
        "elapsed_seconds": elapsed_seconds,
        "operations_per_second": operations_per_second,
        "mib_per_second": mib_per_second,
        "latency_overflow_count": latency_overflow_count,
        **latencies,
    }


def validate_saved_result(point_dir: Path, point: matrix.Point) -> dict:
    try:
        saved = json.loads((point_dir / "result.json").read_text())
    except (FileNotFoundError, json.JSONDecodeError) as error:
        raise ValueError(f"{point.case}: invalid saved result") from error
    if not isinstance(saved, dict) or saved.get("case") != point.case:
        raise ValueError(f"{point.case}: invalid saved result")
    canonical = parse_point(point_dir, point)
    if saved != canonical:
        raise ValueError(f"{point.case}: saved result differs from raw evidence")
    return canonical


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("point_dir", type=Path)
    parser.add_argument("case")
    parser.add_argument("--validate-saved", action="store_true")
    args = parser.parse_args()
    point = matrix.point_by_case(args.case)
    result = (
        validate_saved_result(args.point_dir, point)
        if args.validate_saved
        else parse_point(args.point_dir, point)
    )
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
