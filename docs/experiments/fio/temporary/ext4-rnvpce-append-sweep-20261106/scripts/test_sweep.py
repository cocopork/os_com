#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import importlib.util
from pathlib import Path
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("append_sweep", SCRIPT_DIR / "sweep.py")
sweep = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sweep)


class AppendSweepContractTests(unittest.TestCase):
    def test_matrix_has_two_modes_and_two_nine_point_axes(self):
        points = sweep.matrix()
        self.assertEqual(len(points), 36)
        self.assertEqual({point.mode for point in points}, {"ext4", "rnvpce"})
        self.assertEqual(
            [point.jobs for point in points if point.mode == "rnvpce" and point.axis == "threads"],
            [1, 4, 8, 12, 16, 20, 24, 28, 32],
        )
        self.assertEqual(
            [point.bs_kib for point in points if point.mode == "rnvpce" and point.axis == "bs"],
            [4, 8, 16, 32, 64, 128, 256, 512, 1024],
        )

    def test_thread_point_has_exact_integer_byte_contract(self):
        point = sweep.Point("rnvpce", "threads", jobs=12, bs_kib=4)
        self.assertEqual(point.per_job_mib, 170)
        self.assertEqual(point.actual_mib, 2040)
        self.assertEqual(point.expected_bytes, 2040 * 1024 * 1024)

    def test_layout_validation_records_fragmentation_without_rejecting_it(self):
        rows = [
            {"file": "f.0", "size_bytes": 1024 * 1024**2, "data_extents": 6000, "extent_index_blocks": 20},
            {"file": "f.1", "size_bytes": 1024 * 1024**2, "data_extents": 6100, "extent_index_blocks": 21},
        ]
        summary = sweep.validate_layout(rows, expected_files=2, per_file_mib=1024)
        self.assertEqual(summary["max_data_extents"], 6100)
        self.assertEqual(summary["total_extent_index_blocks"], 41)


if __name__ == "__main__":
    unittest.main()
