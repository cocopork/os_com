# SPDX-License-Identifier: MPL-2.0
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sweep.py")
SPEC = importlib.util.spec_from_file_location("full_e2e_sweep", SCRIPT)
sweep = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sweep)


class MatrixTests(unittest.TestCase):
    def test_qemu_uses_host_pid_namespace_for_iothread_affinity(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertRegex(runner, r'docker run -d --name "\$qemu_name" --pid=host')

    def test_device_whitelist_uses_exact_partition_sizes(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn("nvme) printf '%s\\n' 274877906944", runner)
        self.assertIn("sata) printf '%s\\n' 274877890560", runner)

    def test_full_matrix_has_648_unique_points(self):
        points = sweep.matrix()

        self.assertEqual(len(points), 648)
        self.assertEqual(len({point.case for point in points}), 648)
        self.assertEqual({point.device for point in points}, {"nvme", "sata"})
        self.assertEqual({point.mode for point in points}, {"ext4", "rnvpce"})
        self.assertEqual(
            {point.workload for point in points},
            {"overwrite-randwrite", "append-write", "cold-randread"},
        )
        self.assertEqual({point.repeat for point in points}, {1, 2, 3})

    def test_axis_contracts(self):
        for point in sweep.matrix():
            if point.axis == "threads":
                self.assertEqual(point.bs_kib, 4)
                self.assertIn(point.jobs, sweep.THREADS)
            else:
                self.assertEqual(point.axis, "bs")
                self.assertEqual(point.jobs, 1)
                self.assertIn(point.bs_kib, sweep.BLOCK_KIB)

    def test_format_and_operation_contracts(self):
        for point in sweep.matrix():
            self.assertEqual(point.disk_format, "journal" if point.mode == "ext4" else "nojournal")
            self.assertEqual(point.fio_rw, {
                "overwrite-randwrite": "randwrite",
                "append-write": "write",
                "cold-randread": "randread",
            }[point.workload])
            self.assertEqual(point.prepare, point.workload != "append-write")
            self.assertEqual(point.per_job_mib * point.jobs, point.actual_mib)
            self.assertLessEqual(point.actual_mib, 2048)

    def test_pagecache_regression_matrix_has_requested_focused_points(self):
        points = sweep.pagecache_regression_matrix()

        self.assertEqual(len(points), 24)
        self.assertEqual(len({point.case for point in points}), 24)
        self.assertEqual({point.repeat for point in points}, {1, 2, 3})
        coordinates = {
            (point.workload, point.bs_kib, point.jobs)
            for point in points
            if point.repeat == 1
        }
        self.assertEqual(
            coordinates,
            {
                ("append-write", 128, 1),
                ("append-write", 128, 16),
                ("overwrite-randwrite", 4, 1),
                ("overwrite-randwrite", 4, 16),
                ("cold-randread", 4, 1),
                ("cold-randread", 16, 1),
                ("cold-randread", 64, 1),
                ("cold-randread", 256, 1),
            },
        )
        self.assertTrue(all(point.device == "nvme" for point in points))
        self.assertTrue(all(point.mode == "rnvpce" for point in points))
        self.assertTrue(all(point.axis == "pagecache-regression" for point in points))
        self.assertTrue(all(f"{point.bs}-j{point.jobs}" in point.case for point in points))


class ResultTests(unittest.TestCase):
    def write_fixture(self, root, point, *, operation, bytes_done=None, check_status=0):
        bytes_done = point.expected_bytes if bytes_done is None else bytes_done
        empty = {"io_bytes": 0, "bw_bytes": 0, "iops": 0, "lat_ns": {"mean": 0}, "clat_ns": {"percentile": {}}}
        measured = {
            "io_bytes": bytes_done,
            "bw_bytes": 128 * sweep.MIB,
            "iops": 32768,
            "lat_ns": {"mean": 50000},
            "clat_ns": {"percentile": {"99.000000": 80000}},
        }
        payload = {"jobs": [{"error": 0, "read": measured if operation == "read" else empty,
                              "write": measured if operation == "write" else empty}]}
        (root / "qemu.log").write_text(
            f"FULL_E2E_FIO_START {point.case}\n{json.dumps(payload)}\n"
            f"FULL_E2E_FIO_END {point.case}\n"
            f"FULL_E2E_COMPLETE case={point.case} fio_status=0 sync_status=0 "
            "umount_status=0 start_ns=1000000000 fio_end_ns=3000000000 sync_end_ns=5000000000\n"
        )
        (root / "e2fsck.status").write_text(str(check_status))

    def test_parser_uses_read_section_for_randread(self):
        point = sweep.Point("nvme", "rnvpce", "cold-randread", "bs", 1, 4, 1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, point, operation="read")
            row = sweep.parse_result(root, point)
        self.assertEqual(row["fio_mib_s"], 128)
        self.assertIsNone(row["including_sync_mib_s"])

    def test_parser_rejects_incomplete_io(self):
        point = sweep.Point("sata", "rnvpc", "overwrite-randwrite", "threads", 4, 4, 3)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, point, operation="write", bytes_done=4096)
            with self.assertRaisesRegex(ValueError, "byte count"):
                sweep.parse_result(root, point)


if __name__ == "__main__":
    unittest.main()
