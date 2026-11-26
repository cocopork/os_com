#!/usr/bin/env python3
"""Shared safety and result contracts for the NVLog comparison runners."""

from __future__ import annotations

import json
from pathlib import Path


DEVICE_SIZES = {
    "/dev/nvme1n1p2": 274877906944,
    "/dev/sda4": 274877890560,
}


def nvpc_config(nvm_mib: int) -> str:
    if nvm_mib <= 0:
        raise ValueError("NVM capacity must be positive")
    pages = nvm_mib * 1024 * 1024 // 4096
    return "\n".join(
        (
            "extend_lru=1",
            "absorb_syn=1",
            "promote_level=4",
            "demote_before_promote=1",
            "nvpc_lru_evict=1",
            f"nvpc_sz={pages}",
            "rebuild=0",
            "force_start=1",
            "",
        )
    )


def validate_device_contract(
    *,
    canonical_path: str,
    actual_size: int,
    mounted: bool,
    swap: bool,
    holders: tuple[str, ...],
    open_users: tuple[str, ...],
) -> None:
    if canonical_path not in DEVICE_SIZES:
        raise ValueError(f"device is not allowlisted: {canonical_path}")
    if actual_size != DEVICE_SIZES[canonical_path]:
        raise ValueError(
            f"unexpected size for {canonical_path}: {actual_size}, "
            f"expected {DEVICE_SIZES[canonical_path]}"
        )
    if mounted:
        raise ValueError(f"device is mounted: {canonical_path}")
    if swap:
        raise ValueError(f"device is swap: {canonical_path}")
    if holders:
        raise ValueError(f"device has holders: {', '.join(holders)}")
    if open_users:
        raise ValueError(f"device has open users: {', '.join(open_users)}")


def cache_contract(
    *,
    anon_baseline_bytes: int,
    file_budget_mib: int,
    headroom_mib: int,
    hard_limit_mib: int,
) -> dict[str, int]:
    if min(anon_baseline_bytes, file_budget_mib, headroom_mib, hard_limit_mib) < 0:
        raise ValueError("cache contract values must be non-negative")
    mib = 1024 * 1024
    memory_high_bytes = anon_baseline_bytes + (file_budget_mib + headroom_mib) * mib
    memory_max_bytes = hard_limit_mib * mib
    if memory_max_bytes < memory_high_bytes:
        raise ValueError("hard limit must not be below the cache throttle")
    return {
        "anon_baseline_bytes": anon_baseline_bytes,
        "target_file_bytes": file_budget_mib * mib,
        "headroom_bytes": headroom_mib * mib,
        "memory_high_bytes": memory_high_bytes,
        "memory_max_bytes": memory_max_bytes,
        "allowed_file_deviation_percent": 10,
    }


def result_is_complete(point_dir: Path) -> bool:
    try:
        payload = json.loads((point_dir / "result.json").read_text())
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return False
    return all(
        payload.get(key) == 0
        for key in (
            "benchmark_exit",
            "sync_exit",
            "unmount_exit",
            "qemu_exit",
            "e2fsck_exit",
        )
    ) and payload.get("parse_valid") is True
