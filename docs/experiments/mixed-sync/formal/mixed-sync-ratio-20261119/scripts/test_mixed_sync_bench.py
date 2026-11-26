# SPDX-License-Identifier: MPL-2.0

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
BINARY = SCRIPT_DIR / "mixed_sync_bench"


class MixedSyncBenchTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        subprocess.run(
            ["make", "-C", str(SCRIPT_DIR), "mixed_sync_bench"],
            check=True,
            text=True,
        )

    def run_bench(self, *arguments: str, check: bool = True) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(BINARY), *arguments],
            check=check,
            capture_output=True,
            text=True,
        )

    def test_self_test_proves_independent_reproducible_decisions(self) -> None:
        first = json.loads(self.run_bench("--self-test").stdout)
        second = json.loads(self.run_bench("--self-test").stdout)

        self.assertEqual(first, second)
        self.assertEqual(first["status"], "pass")
        self.assertEqual(first["decision_count"], 10000)
        self.assertEqual(first["sync_counts"]["0"], 0)
        self.assertLessEqual(first["sync_counts"]["20"], first["sync_counts"]["40"])
        self.assertLessEqual(first["sync_counts"]["40"], first["sync_counts"]["60"])
        self.assertLessEqual(first["sync_counts"]["60"], first["sync_counts"]["80"])
        self.assertEqual(first["sync_counts"]["100"], first["write_count"])
        self.assertEqual(first["address_digest"], 17199379227451352568)
        self.assertEqual(first["operation_digest"], 2011287794019180530)
        self.assertEqual(
            first["sync_digests"],
            {
                "0": 1469598103934665603,
                "20": 6006194862186044860,
                "40": 14209598604338577549,
                "60": 5877748052607373086,
                "80": 12154269550461606499,
                "100": 13382538412120072467,
            },
        )

    def test_guest_binary_is_statically_linked(self) -> None:
        description = subprocess.check_output(["file", str(BINARY)], text=True)
        self.assertIn("statically linked", description)

    def test_rejects_invalid_percentage(self) -> None:
        result = self.run_bench("--read-percent", "101", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("read-percent", result.stderr)

    def test_requires_one_existing_file_per_thread(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "f.0").write_bytes(bytes(4096))
            result = self.run_bench(
                "--directory",
                directory,
                "--threads",
                "2",
                "--file-bytes",
                "4096",
                "--warmup-seconds",
                "0",
                "--runtime-seconds",
                "1",
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("f.1", result.stderr)

    def test_rejects_a_correctly_sized_sparse_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "f.0"
            with path.open("wb") as output:
                output.truncate(1024 * 1024)
            result = self.run_bench(
                "--directory",
                directory,
                "--threads",
                "1",
                "--file-bytes",
                str(1024 * 1024),
                "--runtime-seconds",
                "1",
                "--warmup-seconds",
                "0",
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fully allocated", result.stderr)

    def test_partial_thread_startup_failure_exits_without_hanging(self) -> None:
        subprocess.run(
            ["make", "-C", str(SCRIPT_DIR), "mixed_sync_bench_fault_create"],
            check=True,
        )
        with tempfile.TemporaryDirectory() as directory:
            for thread_id in range(2):
                (Path(directory) / f"f.{thread_id}").write_bytes(bytes(1024 * 1024))
            result = subprocess.run(
                [
                    str(SCRIPT_DIR / "mixed_sync_bench_fault_create"),
                    "--directory",
                    directory,
                    "--threads",
                    "2",
                    "--file-bytes",
                    str(1024 * 1024),
                    "--runtime-seconds",
                    "1",
                    "--warmup-seconds",
                    "0",
                ],
                check=False,
                capture_output=True,
                text=True,
                timeout=2,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pthread_create", result.stderr)

    def test_long_io_latency_is_not_capped_at_65535_microseconds(self) -> None:
        subprocess.run(
            ["make", "-C", str(SCRIPT_DIR), "mixed_sync_bench_slow_io"],
            check=True,
        )
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "f.0").write_bytes(bytes(1024 * 1024))
            result = subprocess.run(
                [
                    str(SCRIPT_DIR / "mixed_sync_bench_slow_io"),
                    "--directory",
                    directory,
                    "--threads",
                    "1",
                    "--file-bytes",
                    str(1024 * 1024),
                    "--read-percent",
                    "100",
                    "--runtime-seconds",
                    "1",
                    "--warmup-seconds",
                    "0",
                ],
                check=True,
                capture_output=True,
                text=True,
            )
        payload = json.loads(result.stdout)
        self.assertGreaterEqual(payload["p50_latency_us"], 90000)
        self.assertGreaterEqual(payload["p99_latency_us"], 90000)
        self.assertEqual(payload["latency_overflow_count"], 0)

    def test_runs_real_mixed_io_and_emits_consistent_json(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            for thread_id in range(2):
                (Path(directory) / f"f.{thread_id}").write_bytes(bytes(1024 * 1024))
            result = self.run_bench(
                "--directory",
                directory,
                "--threads",
                "2",
                "--file-bytes",
                str(1024 * 1024),
                "--read-percent",
                "50",
                "--sync-percent",
                "40",
                "--seed",
                "20261119",
                "--warmup-seconds",
                "0",
                "--runtime-seconds",
                "1",
            )

        payload = json.loads(result.stdout)
        self.assertEqual(payload["threads"], 2)
        self.assertEqual(payload["file_bytes"], 1024 * 1024)
        self.assertEqual(payload["read_percent"], 50)
        self.assertEqual(payload["sync_percent"], 40)
        self.assertEqual(payload["seed"], 20261119)
        self.assertGreater(payload["operations"], 0)
        self.assertEqual(payload["operations"], payload["reads"] + payload["writes"])
        self.assertLessEqual(payload["fsyncs"], payload["writes"])
        self.assertGreater(payload["elapsed_seconds"], 0.0)
        self.assertGreater(payload["operations_per_second"], 0.0)
        for field in ("mean_latency_us", "p50_latency_us", "p95_latency_us", "p99_latency_us", "p999_latency_us"):
            self.assertGreaterEqual(payload[field], 0.0)


if __name__ == "__main__":
    unittest.main()
