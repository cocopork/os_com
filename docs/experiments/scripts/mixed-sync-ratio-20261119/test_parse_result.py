# SPDX-License-Identifier: MPL-2.0

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import matrix
import parse_result


class ParseResultTest(unittest.TestCase):
    def setUp(self) -> None:
        self.point = matrix.Point("nvme", "rnvpce", 30, 40, 1)
        self.payload = {
            "threads": 8,
            "files_opened": 8,
            "file_bytes": 335544320,
            "read_percent": 30,
            "sync_percent": 40,
            "seed": 20261119,
            "warmup_seconds": 10,
            "runtime_seconds": 30,
            "elapsed_seconds": 30.001,
            "operations": 10000,
            "reads": 3000,
            "writes": 7000,
            "fsyncs": 2800,
            "read_bytes": 12288000,
            "write_bytes": 28672000,
            "operations_per_second": 333.322,
            "mib_per_second": 1.302,
            "mean_latency_us": 20000.0,
            "p50_latency_us": 1000,
            "p95_latency_us": 2000,
            "p99_latency_us": 3000,
            "p999_latency_us": 4000,
        }

    def write_fixture(
        self,
        root: Path,
        payload: dict | None = None,
        *,
        prewrite_files: int = 8,
        complete: bool = True,
        benchmark_status: int = 0,
        e2fsck_status: int = 0,
    ) -> Path:
        payload = payload or self.payload
        lines = [
            *(
                f"ACTION PREWRITE_FILE thread={thread} path=/mnt/data/f.{thread} bytes=335544320"
                for thread in range(prewrite_files)
            ),
            (
                f"MIXED_SYNC_CONFIG case={self.point.case} device=nvme mode=rnvpce "
                "read_percent=30 sync_percent=40 repeat=1 threads=8 file_mib=320 "
                "dataset_mib=2560 seed=20261119 warmup_seconds=10 runtime_seconds=30 "
                "cache_control=strict configured_dram_cache_mib=128 "
                "configured_nv_data_cache_mib=128 configured_total_data_cache_mib=256"
            ),
            "console=ttyS0 ostd.log_level=error rtio_cpu_trace=off",
            f"MIXED_SYNC_JSON_BEGIN case={self.point.case}",
            json.dumps(payload),
            f"MIXED_SYNC_JSON_END case={self.point.case}",
        ]
        if complete:
            lines.append(
                f"MIXED_SYNC_COMPLETE case={self.point.case} "
                f"benchmark_status={benchmark_status} sync_status=0 umount_status=0"
            )
        (root / "qemu.log").write_text("\n".join(lines) + "\n")
        (root / "e2fsck.status").write_text(f"{e2fsck_status}\n")
        return root

    def test_parses_valid_result_and_derives_actual_ratios(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            result = parse_result.parse_point(
                self.write_fixture(Path(directory)), self.point
            )

        self.assertEqual(result["case"], self.point.case)
        self.assertEqual(result["prewrite_bytes"], 8 * 335544320)
        self.assertEqual(result["actual_read_percent"], 30.0)
        self.assertEqual(result["actual_sync_percent"], 40.0)
        self.assertEqual(result["cache_control"], "strict")

    def test_rejects_wrong_seed(self) -> None:
        payload = {**self.payload, "seed": 7}
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "seed"):
                parse_result.parse_point(
                    self.write_fixture(Path(directory), payload), self.point
                )

    def test_rejects_count_and_ratio_inconsistency(self) -> None:
        variants = (
            ({**self.payload, "operations": 9999}, "operation counts"),
            ({**self.payload, "reads": 9000, "writes": 1000, "fsyncs": 400, "read_bytes": 36864000, "write_bytes": 4096000}, "read ratio"),
            ({**self.payload, "fsyncs": 7000}, "sync ratio"),
        )
        for payload, message in variants:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(ValueError, message):
                    parse_result.parse_point(
                        self.write_fixture(Path(directory), payload), self.point
                    )

    def test_rejects_incomplete_prewrite_cleanup_and_filesystem_check(self) -> None:
        variants = (
            ({"prewrite_files": 7}, "prewrite"),
            ({"complete": False}, "completion"),
            ({"benchmark_status": 1}, "benchmark_status"),
            ({"e2fsck_status": 4}, "e2fsck"),
        )
        for options, message in variants:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(ValueError, message):
                    parse_result.parse_point(
                        self.write_fixture(Path(directory), self.payload, **options), self.point
                    )


class RunnerAuditTest(unittest.TestCase):
    def test_audit_resolves_exact_device_and_queue_plan_without_device_access(self) -> None:
        case = "mixed-nvme-rnvpce-r30w70-sync40-r1"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            results = root / "results"
            source.mkdir()
            results.mkdir()
            for name in ("selected.iso", "ext2.img", "exfat.img"):
                (source / name).write_bytes(b"audit")
            environment = os.environ.copy()
            environment.update(
                ROOT_ART=str(results),
                SOURCE_ART=str(source),
                CASES=case,
                MIXED_SYNC_RUNNER_AUDIT="1",
            )
            result = subprocess.run(
                ["bash", str(SCRIPT_DIR / "run-sweep.sh")],
                check=True,
                capture_output=True,
                text=True,
                env=environment,
            )

        self.assertIn(f"AUDIT case={case}", result.stdout)
        self.assertIn("target=/dev/nvme1n1p2", result.stdout)
        self.assertIn("virtqueues=8", result.stdout)
        self.assertIn("iothread_cpus=56-63", result.stdout)
        self.assertIn("profiling=off", result.stdout)


if __name__ == "__main__":
    unittest.main()
