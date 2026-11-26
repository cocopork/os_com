#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import importlib.util
from pathlib import Path
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("overwrite_sweep", SCRIPT_DIR / "sweep.py")
sweep = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sweep)


class SweepContractTests(unittest.TestCase):
    def test_matrix_contains_two_modes_two_workloads_and_both_nine_point_axes(self):
        points = sweep.matrix()
        self.assertEqual(len(points), 72)
        self.assertEqual({point.mode for point in points}, {"ext4", "rnvpce"})
        self.assertEqual({point.workload for point in points}, {"randwrite", "write"})
        self.assertEqual(
            [point.jobs for point in points if point.mode == "rnvpce" and point.workload == "randwrite" and point.axis == "threads"],
            [1, 4, 8, 12, 16, 20, 24, 28, 32],
        )
        self.assertEqual(
            [point.bs_kib for point in points if point.mode == "rnvpce" and point.workload == "write" and point.axis == "bs"],
            [4, 8, 16, 32, 64, 128, 256, 512, 1024],
        )

    def test_thread_point_uses_integer_per_inode_size_and_exact_total(self):
        point = sweep.Point("rnvpce", "threads", "randwrite", jobs=12, bs_kib=4)
        self.assertEqual(point.per_job_mib, 170)
        self.assertEqual(point.actual_mib, 2040)
        self.assertEqual(point.expected_bytes, 2040 * 1024 * 1024)

    def test_layout_gate_accepts_low_fragmentation_and_rejects_interleaving(self):
        low = [
            {"file": "f.0", "size_bytes": 1024 * 1024**2, "data_extents": 9, "extent_index_blocks": 1},
            {"file": "f.1", "size_bytes": 1024 * 1024**2, "data_extents": 9, "extent_index_blocks": 1},
        ]
        summary = sweep.validate_layout(low, expected_files=2, per_file_mib=1024)
        self.assertEqual(summary["max_data_extents"], 9)
        fragmented = [dict(row, data_extents=7000) for row in low]
        with self.assertRaisesRegex(ValueError, "fragmented"):
            sweep.validate_layout(fragmented, expected_files=2, per_file_mib=1024)


if __name__ == "__main__":
    unittest.main()
