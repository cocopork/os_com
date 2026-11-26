#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("raw_device_summarize", SCRIPT_DIR / "summarize.py")
summarize = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(summarize)


class RawDeviceSummarizeTest(unittest.TestCase):
    def test_collects_rows_and_aggregates_repetitions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            point = root / "raw-test-point"
            point.mkdir()
            rows = [
                {
                    "case": "raw-nvme-linux-cold-randread-bs-4k",
                    "device": "nvme",
                    "system": "linux",
                    "workload": "cold-randread",
                    "axis": "bs",
                    "bs_kib": 4,
                    "jobs": 1,
                    "repeat": repeat,
                    "actual_mib": 2048,
                    "virtqueues": 8,
                    "fio_mib_s": value,
                    "iops": value * 256,
                    "mean_latency_us": 30,
                    "p99_latency_us": 60,
                }
                for repeat, value in enumerate((100, 110, 120), 1)
            ]
            (point / "result.json").write_text(json.dumps(rows))

            collected = summarize.collect_rows(root)
            aggregated = summarize.aggregate_rows(collected)

        self.assertEqual(len(collected), 3)
        self.assertEqual(len(aggregated), 1)
        self.assertEqual(aggregated[0]["samples"], 3)
        self.assertEqual(aggregated[0]["fio_mib_s_mean"], 110)
        self.assertEqual(aggregated[0]["fio_mib_s_min"], 100)
        self.assertEqual(aggregated[0]["fio_mib_s_max"], 120)


if __name__ == "__main__":
    unittest.main()
