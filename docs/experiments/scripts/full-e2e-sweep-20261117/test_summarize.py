# SPDX-License-Identifier: MPL-2.0
import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("summarize.py")
SPEC = importlib.util.spec_from_file_location("full_e2e_summarize", SCRIPT)
summarize = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(summarize)


class AggregateTests(unittest.TestCase):
    def test_repeats_are_aggregated_without_crossing_device(self):
        rows = []
        for device, base in (("nvme", 100), ("sata", 20)):
            for repeat, delta in enumerate((0, 10, 20), 1):
                rows.append({
                    "device": device, "mode": "rnvpce", "workload": "cold-randread",
                    "axis": "threads", "value": "4", "bs_kib": 4, "jobs": 4,
                    "repeat": repeat, "fio_mib_s": base + delta,
                    "mean_latency_us": 50 + delta, "p99_latency_us": 80 + delta,
                    "including_sync_mib_s": None,
                })
        result = summarize.aggregate(rows)
        self.assertEqual(len(result), 2)
        by_device = {row["device"]: row for row in result}
        self.assertEqual(by_device["nvme"]["fio_mib_s_mean"], 110)
        self.assertEqual(by_device["sata"]["fio_mib_s_mean"], 30)
        self.assertEqual(by_device["nvme"]["samples"], 3)


if __name__ == "__main__":
    unittest.main()
