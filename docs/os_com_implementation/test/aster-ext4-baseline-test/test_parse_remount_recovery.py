from __future__ import annotations

import csv
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("parse_remount_recovery.py")


def run_parser(tmp_path: Path, lines: list[str]) -> tuple[list[dict], list[dict]]:
    log = tmp_path / "run.log"
    raw = tmp_path / "raw.csv"
    summary = tmp_path / "summary.csv"
    log.write_text("\n".join(lines) + "\n", encoding="utf-8")
    subprocess.run(
        [sys.executable, str(SCRIPT), str(log), str(raw), str(summary)],
        check=True,
    )
    with raw.open(newline="", encoding="utf-8") as stream:
        raw_rows = list(csv.DictReader(stream))
    with summary.open(newline="", encoding="utf-8") as stream:
        summary_rows = list(csv.DictReader(stream))
    return raw_rows, summary_rows


def marker(
    profile: str,
    size: int,
    repeat: int,
    ms: int,
    valid: int = 1,
    verify_ok: int | None = None,
) -> str:
    verify_field = "" if verify_ok is None else f" verify_ok={verify_ok}"
    return (
        f"=== REMOUNT_RESULT profile={profile} data_mb={size} repeat={repeat} "
        f"remount_ns={ms * 1_000_000} file_size={size * 1024 * 1024} "
        f"valid={valid}{verify_field} ==="
    )


def profile_marker(
    mode: str,
    total_us: int,
    super_options_us: int = 10,
    block_groups_us: int = 20,
    construct_us: int = 30,
    journal_us: int = 40,
    writeback_worker_us: int = 50,
    rnvpc_worker_us: int = 60,
) -> str:
    return (
        f"[MOUNT_PROFILE] mode={mode} total_us={total_us} "
        f"super_options_us={super_options_us} block_groups_us={block_groups_us} "
        f"construct_us={construct_us} journal_us={journal_us} "
        f"writeback_worker_us={writeback_worker_us} rnvpc_worker_us={rnvpc_worker_us}"
    )


class ParseRemountRecoveryTest(unittest.TestCase):
    def test_attaches_first_kernel_profile_after_remount_begin(self) -> None:
        lines = [
            profile_marker("Native", 111),
            "=== REMOUNT_PROFILE_BEGIN profile=ext4-orderless data_mb=32 repeat=1 ===",
            "[    1.234] ERROR: ext4: " + profile_marker("Native", 222),
            marker("ext4-orderless", 32, 1, 10),
            profile_marker("Native", 333),
        ]

        with tempfile.TemporaryDirectory() as directory:
            raw, summary = run_parser(Path(directory), lines)

        self.assertEqual(raw[0]["mount_total_us"], "222")
        self.assertEqual(raw[0]["mount_block_groups_us"], "20")
        self.assertEqual(summary[0]["mount_total_median_us"], "222.000")

    def test_summarizes_six_points_with_nearest_rank_p95(self) -> None:
        lines = []
        for profile in ("ext4-orderless", "ext4-rNVPCe"):
            for size in (32, 256, 2048):
                for repeat, ms in enumerate((10, 12, 14, 16, 18), start=1):
                    lines.append(marker(profile, size, repeat, ms))

        with tempfile.TemporaryDirectory() as directory:
            raw, summary = run_parser(Path(directory), lines)

        self.assertEqual(len(raw), 30)
        self.assertEqual(len(summary), 6)
        self.assertEqual(summary[0]["profile"], "ext4-orderless")
        self.assertEqual(summary[0]["data_mb"], "32")
        self.assertEqual(summary[0]["repeats"], "5")
        self.assertEqual(summary[0]["valid_repeats"], "5")
        self.assertEqual(summary[0]["median_ms"], "14.000")
        self.assertEqual(summary[0]["p95_ms"], "18.000")
        self.assertEqual(summary[0]["min_ms"], "10.000")
        self.assertEqual(summary[0]["max_ms"], "18.000")

    def test_invalid_repeat_is_retained_but_excluded_from_latency(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            raw, summary = run_parser(
                Path(directory),
                [
                    marker("ext4-orderless", 32, 1, 10),
                    marker("ext4-orderless", 32, 2, 999, valid=0),
                    marker("ext4-orderless", 32, 3, 20),
                ],
            )

        self.assertEqual(len(raw), 3)
        self.assertEqual(summary[0]["repeats"], "3")
        self.assertEqual(summary[0]["valid_repeats"], "2")
        self.assertEqual(summary[0]["median_ms"], "15.000")
        self.assertEqual(summary[0]["p95_ms"], "20.000")

    def test_failed_content_verification_is_excluded_from_latency(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            raw, summary = run_parser(
                Path(directory),
                [
                    marker("ext4-rNVPCe", 256, 1, 10, verify_ok=1),
                    marker("ext4-rNVPCe", 256, 2, 20, verify_ok=0),
                ],
            )

        self.assertEqual(raw[0]["verify_ok"], "1")
        self.assertEqual(raw[1]["verify_ok"], "0")
        self.assertEqual(summary[0]["repeats"], "2")
        self.assertEqual(summary[0]["valid_repeats"], "1")
        self.assertEqual(summary[0]["median_ms"], "10.000")


if __name__ == "__main__":
    unittest.main()
