#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

import importlib.util
from pathlib import Path
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("raw_device_matrix", SCRIPT_DIR / "matrix.py")
matrix_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matrix_module)


class RawDeviceMatrixTest(unittest.TestCase):
    def test_matrix_covers_the_aligned_driver_comparison(self):
        points = matrix_module.matrix()

        self.assertEqual(len(points), 216)
        self.assertEqual(len({point.case for point in points}), len(points))
        self.assertEqual({point.system for point in points}, {"asterinas", "linux"})
        self.assertEqual({point.device for point in points}, {"nvme", "sata"})
        self.assertEqual(
            {point.workload for point in points},
            {"append-write", "overwrite-randwrite", "cold-randread"},
        )

    def test_each_system_uses_identical_fio_coordinates(self):
        points = matrix_module.matrix()
        coordinates = {}
        for point in points:
            key = (point.device, point.workload, point.axis, point.bs_kib, point.jobs)
            coordinates.setdefault(key, set()).add(point.system)

        self.assertTrue(coordinates)
        self.assertTrue(all(systems == {"asterinas", "linux"} for systems in coordinates.values()))

    def test_sweeps_use_two_gib_and_three_repetitions(self):
        thread = matrix_module.point_by_case(
            "raw-nvme-asterinas-overwrite-randwrite-threads-12"
        )
        block_size = matrix_module.point_by_case(
            "raw-sata-linux-cold-randread-bs-1024k"
        )

        self.assertEqual(thread.bs_kib, 4)
        self.assertEqual(thread.jobs, 12)
        self.assertEqual(thread.per_job_mib, 170)
        self.assertEqual(thread.actual_mib, 2040)
        self.assertEqual(thread.repeats, 3)
        self.assertEqual(block_size.bs_kib, 1024)
        self.assertEqual(block_size.jobs, 1)
        self.assertEqual(block_size.actual_mib, 2048)
        self.assertEqual(block_size.repeats, 3)

    def test_workloads_map_to_raw_fio_operations(self):
        expected = {
            "append-write": "write",
            "overwrite-randwrite": "randwrite",
            "cold-randread": "randread",
        }
        for point in matrix_module.matrix():
            self.assertEqual(point.fio_rw, expected[point.workload])

    def test_device_queue_profile_is_explicit(self):
        self.assertEqual(matrix_module.virtqueues("nvme"), 8)
        self.assertEqual(matrix_module.virtqueues("sata"), 1)


if __name__ == "__main__":
    unittest.main()
