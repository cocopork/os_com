# SPDX-License-Identifier: MPL-2.0

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
MATRIX_PATH = SCRIPT_DIR / "matrix.py"
GUEST_PATH = SCRIPT_DIR / "guest.sh"


def load_matrix():
    spec = importlib.util.spec_from_file_location("mixed_sync_matrix", MATRIX_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class MatrixTest(unittest.TestCase):
    def test_matrix_contains_every_coordinate_exactly_three_times(self) -> None:
        module = load_matrix()
        points = module.matrix()

        self.assertEqual(len(points), 432)
        self.assertEqual(len({point.case for point in points}), 432)
        self.assertEqual({point.device for point in points}, {"nvme", "sata"})
        self.assertEqual({point.mode for point in points}, {"ext2", "ext4", "rnvpce"})
        self.assertEqual({point.read_percent for point in points}, {0, 30, 50, 70})
        self.assertEqual({point.sync_percent for point in points}, {0, 20, 40, 60, 80, 100})
        self.assertEqual({point.repeat for point in points}, {1, 2, 3})
        self.assertTrue(all(point.threads == 8 for point in points))
        self.assertTrue(all(point.file_mib == 320 for point in points))
        self.assertTrue(all(point.dataset_mib == 2560 for point in points))
        self.assertEqual({point.virtqueues for point in points if point.device == "nvme"}, {8})
        self.assertEqual({point.virtqueues for point in points if point.device == "sata"}, {1})

    def test_case_lookup_is_exact(self) -> None:
        module = load_matrix()
        point = module.matrix()[123]
        self.assertEqual(module.point_by_case(point.case), point)
        with self.assertRaisesRegex(ValueError, "unknown case"):
            module.point_by_case(point.case + "-typo")

    def test_guest_audit_runs_prewrite_measurement_and_cleanup_flow(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "case.cfg"
            config.write_text(
                "\n".join(
                    (
                        "CASE=test-case",
                        "MODE=rnvpce",
                        "DEVICE=nvme",
                        "READ_PERCENT=30",
                        "SYNC_PERCENT=40",
                        "REPEAT=1",
                        "THREADS=8",
                        "FILE_MIB=320",
                        "FILE_BYTES=335544320",
                        "DATASET_MIB=2560",
                        "SEED=20261119",
                        "WARMUP_SECONDS=10",
                        "RUNTIME_SECONDS=30",
                        "DRAM_CACHE_MIB=128",
                        "NV_DATA_CACHE_MIB=128",
                    )
                )
                + "\n"
            )
            environment = os.environ.copy()
            environment.update(
                CONFIG_PATH=str(config),
                MIXED_SYNC_AUDIT="1",
                BENCHMARK="/test/mixed_sync_bench",
                MOUNT_POINT="/test/mnt",
                TARGET_DEVICE="/dev/test-target",
            )
            result = subprocess.run(
                ["sh", str(GUEST_PATH)],
                check=True,
                capture_output=True,
                text=True,
                env=environment,
            )

        actions = [line for line in result.stdout.splitlines() if line.startswith("ACTION ")]
        prewrites = [line for line in actions if "PREWRITE_FILE" in line]
        self.assertEqual(len(prewrites), 8)
        self.assertIn("bytes=335544320", prewrites[0])
        prepare_mount = next(line for line in actions if "PREPARE_MOUNT" in line)
        self.assertIn("type=ext4", prepare_mount)
        self.assertIn("rnvpc_extend=off", prepare_mount)
        self.assertLess(actions.index("ACTION PREPARE_SYNC"), actions.index("ACTION PREPARE_UMOUNT"))
        measured_mount = next(line for line in actions if "MEASURE_MOUNT" in line)
        self.assertIn("pagecache_capacity_mb=128", measured_mount)
        self.assertIn("rnvpc_extend=on", measured_mount)
        benchmark = next(line for line in actions if "BENCHMARK" in line)
        self.assertIn("read_percent=30", benchmark)
        self.assertIn("sync_percent=40", benchmark)
        self.assertIn("warmup_seconds=10", benchmark)
        self.assertIn("runtime_seconds=30", benchmark)
        self.assertEqual(actions[-2:], ["ACTION FINAL_SYNC", "ACTION FINAL_UMOUNT"])
        self.assertIn("cache_control=strict", result.stdout)
        self.assertIn("configured_total_data_cache_mib=256", result.stdout)


if __name__ == "__main__":
    unittest.main()
