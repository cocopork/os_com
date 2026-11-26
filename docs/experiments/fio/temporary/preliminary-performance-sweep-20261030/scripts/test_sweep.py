#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import json
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

import sweep


class MatrixTests(unittest.TestCase):
    def test_matrix_has_all_unique_points(self):
        points = sweep.matrix()
        self.assertEqual(len(points), 108)
        self.assertEqual(len({point.case for point in points}), 108)
        self.assertEqual({point.mode for point in points}, {"ext4", "rnvpc", "rnvpce"})
        self.assertEqual({point.workload for point in points}, {"randwrite", "write"})
        self.assertEqual({point.axis for point in points}, {"threads", "bs"})

    def test_axes_and_floor_divided_bytes(self):
        points = sweep.matrix()
        thread_values = {point.jobs for point in points if point.axis == "threads"}
        block_values = {point.bs_kib for point in points if point.axis == "bs"}
        self.assertEqual(thread_values, {1, 4, 8, 12, 16, 20, 24, 28, 32})
        self.assertEqual(block_values, {4, 8, 16, 32, 64, 128, 256, 512, 1024})
        point = next(p for p in points if p.axis == "threads" and p.jobs == 12)
        self.assertEqual(point.per_job_mib, 170)
        self.assertEqual(point.expected_bytes, 2040 * 1024 * 1024)


class ParserTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.artifacts = Path(self.tmp.name)
        self.point = next(
            point
            for point in sweep.matrix()
            if point.mode == "rnvpc" and point.axis == "threads"
            and point.workload == "randwrite" and point.jobs == 4
        )

    def tearDown(self):
        self.tmp.cleanup()

    def write_valid_evidence(self, point=None):
        point = point or self.point
        job = {
            "error": 0,
            "job options": {
                "rw": point.workload,
                "bs": point.bs,
                "numjobs": str(point.jobs),
                "direct": "0",
                "ioengine": "psync",
            },
            "write": {
                "io_bytes": point.expected_bytes,
                "bw_bytes": 200 * 1024 * 1024,
                "runtime": 10000,
                "clat_ns": {
                    "mean": 5000.0,
                    "percentile": {"99.000000": 9000.0},
                },
            },
        }
        payload = {"jobs": [job]}
        prep = (
            f"PRELIM_PREPARE_COMPLETE {point.case} bytes={point.expected_bytes} status=0\n"
            if point.workload == "randwrite"
            else f"PRELIM_PREPARE_COMPLETE {point.case} bytes=0 status=0\n"
        )
        text = (
            f"PRELIM_CONFIG case={point.case} mode={point.mode} axis={point.axis} "
            f"workload={point.workload} bs={point.bs} jobs={point.jobs} "
            f"total_mib={point.total_mib} expected_bytes={point.expected_bytes}\n"
            f"PRELIM_JOURNAL case={point.case} disk={int(point.disk_journal)} "
            f"runtime={int(point.runtime_journal)}\n"
            + prep
            + f"PRELIM_FIO_START {point.case}\n{json.dumps(payload)}\n"
            + f"PRELIM_FIO_END {point.case}\n"
            + f"PRELIM_COMPLETE {point.case} fio_status=0 start_ns=1000000000 "
            "fio_end_ns=11000000000 sync_end_ns=12000000000 umount_status=0\n"
        )
        (self.artifacts / f"{point.case}.log").write_text(text)
        (self.artifacts / f"{point.case}-e2fsck.log").write_text(
            "Pass 1: Checking inodes, blocks, and sizes\n"
            "Pass 5: Checking group summary information\n"
            "/dev/benchmark-target: clean, 12/100 files, 100/1000 blocks\n"
            "PRELIM_E2FSCK_STATUS 0\n"
        )

    def test_valid_point(self):
        self.write_valid_evidence()
        row = sweep.parse_artifacts(self.artifacts, self.point)
        self.assertEqual(row["outcome"], "complete")
        self.assertEqual(row["io_mib"], self.point.expected_bytes / 1048576)
        self.assertEqual(row["sync_mib_s"], self.point.expected_bytes / 1048576 / 11)

    def test_rejects_missing_or_inconsistent_conditions(self):
        mutations = {
            "completion": ("PRELIM_COMPLETE", "PRELIM_BROKEN"),
            "journal": ("disk=0 runtime=0", "disk=1 runtime=1"),
            "bytes": (str(self.point.expected_bytes), str(self.point.expected_bytes - 4096)),
            "unmount": ("umount_status=0", "umount_status=1"),
            "prepare": ("PRELIM_PREPARE_COMPLETE", "PRELIM_PREPARE_BROKEN"),
        }
        for name, (old, new) in mutations.items():
            with self.subTest(name=name):
                self.write_valid_evidence()
                path = self.artifacts / f"{self.point.case}.log"
                path.write_text(path.read_text().replace(old, new, 1))
                with self.assertRaises(ValueError):
                    sweep.parse_artifacts(self.artifacts, self.point)

    def test_rejects_failed_or_missing_e2fsck(self):
        self.write_valid_evidence()
        check = self.artifacts / f"{self.point.case}-e2fsck.log"
        check.write_text(check.read_text().replace("PRELIM_E2FSCK_STATUS 0", "PRELIM_E2FSCK_STATUS 4"))
        with self.assertRaises(ValueError):
            sweep.parse_artifacts(self.artifacts, self.point)
        check.unlink()
        with self.assertRaises(ValueError):
            sweep.parse_artifacts(self.artifacts, self.point)

    def test_rejects_wrong_fio_contract(self):
        self.write_valid_evidence()
        path = self.artifacts / f"{self.point.case}.log"
        path.write_text(path.read_text().replace('"direct": "0"', '"direct": "1"'))
        with self.assertRaises(ValueError):
            sweep.parse_artifacts(self.artifacts, self.point)

    def test_sequential_point_requires_no_prefill_bytes(self):
        point = replace(self.point, workload="write")
        self.write_valid_evidence(point)
        row = sweep.parse_artifacts(self.artifacts, point)
        self.assertEqual(row["outcome"], "complete")


if __name__ == "__main__":
    unittest.main()
