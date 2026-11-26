#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent


def load_module(name: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


matrix = load_module("matrix")
results = load_module("results")


def fio_payload(operation: str, io_bytes: int) -> dict:
    idle = {"io_bytes": 0, "bw_bytes": 0, "iops": 0, "lat_ns": {"mean": 0}, "clat_ns": {}}
    measured = {
        "io_bytes": io_bytes,
        "bw_bytes": 128 * 1024 * 1024,
        "iops": 32768,
        "lat_ns": {"mean": 30000},
        "clat_ns": {"percentile": {"99.000000": 60000}},
    }
    return {"jobs": [{"error": 0, "read": measured if operation == "read" else idle,
                       "write": measured if operation == "write" else idle}]}


class RawDeviceResultsTest(unittest.TestCase):
    def write_log(self, root: Path, point, repeats: int = 3, io_bytes: int | None = None):
        operation = "read" if point.fio_rw == "randread" else "write"
        sections = []
        for repeat in range(1, repeats + 1):
            payload = fio_payload(operation, point.expected_bytes if io_bytes is None else io_bytes)
            sections.extend(
                [
                    f"RAW_DEVICE_FIO_START case={point.case} repeat={repeat}",
                    json.dumps(payload),
                    f"RAW_DEVICE_FIO_END case={point.case} repeat={repeat}",
                ]
            )
        sections.append(
            f"RAW_DEVICE_COMPLETE case={point.case} fio_status=0 completed_repeats={repeats}"
        )
        (root / "qemu.log").write_text("\n".join(sections) + "\n")

    def test_parses_all_three_repetitions(self):
        point = matrix.point_by_case("raw-nvme-linux-cold-randread-bs-1024k")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_log(root, point)
            rows = results.parse_point(root, point)

        self.assertEqual([row["repeat"] for row in rows], [1, 2, 3])
        self.assertTrue(all(row["fio_mib_s"] == 128 for row in rows))
        self.assertTrue(all(row["actual_mib"] == 2048 for row in rows))
        self.assertTrue(all(row["virtqueues"] == 8 for row in rows))

    def test_rejects_a_missing_repetition(self):
        point = matrix.point_by_case("raw-sata-asterinas-append-write-threads-4")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_log(root, point, repeats=2)
            with self.assertRaisesRegex(ValueError, "completed_repeats"):
                results.parse_point(root, point)

    def test_rejects_short_io(self):
        point = matrix.point_by_case("raw-nvme-asterinas-overwrite-randwrite-threads-1")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_log(root, point, io_bytes=point.expected_bytes - 4096)
            with self.assertRaisesRegex(ValueError, "byte count"):
                results.parse_point(root, point)


if __name__ == "__main__":
    unittest.main()
