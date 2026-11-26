# SPDX-License-Identifier: MPL-2.0
import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("summarize.py")
SPEC = importlib.util.spec_from_file_location("full_e2e_summarize", SCRIPT)
summarize = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(summarize)


class AggregateTests(unittest.TestCase):
    def test_empty_csv_metric_is_treated_as_missing(self):
        row = {
            "device": "nvme", "mode": "nvlog", "workload": "cold-randread",
            "axis": "bs", "value": "4k", "bs_kib": 4, "jobs": 1,
            "fio_mib_s": "100", "including_sync_mib_s": "",
            "mean_latency_us": "10", "p99_latency_us": "20",
        }
        result = summarize.aggregate([row])
        self.assertIsNone(result[0]["including_sync_mib_s_mean"])

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

    def test_normalizes_nvlog_result_to_existing_csv_contract(self):
        normalizer = getattr(summarize, "normalize_nvlog_result", None)
        self.assertIsNotNone(normalizer)
        payload = {
            "case": "e2e-nvme-nvlog-overwrite-randwrite-threads-4-r2",
            "device": "nvme",
            "mode": "nvlog",
            "workload": "overwrite-randwrite",
            "axis": "threads",
            "value": "4",
            "bs_kib": 4,
            "jobs": 4,
            "repeat": 2,
            "actual_mib": 2048,
            "fio_mib_s": 500.0,
            "including_sync_mib_s": 450.0,
            "iops": 128000.0,
            "mean_latency_us": 30.0,
            "p99_latency_us": 80.0,
        }
        row = normalizer(payload)
        self.assertEqual(
            list(row),
            [
                "case", "device", "mode", "workload", "axis", "value",
                "bs_kib", "jobs", "repeat", "actual_mib", "fio_mib_s",
                "including_sync_mib_s", "iops", "mean_latency_us",
                "p99_latency_us",
            ],
        )
        self.assertEqual(row["mode"], "nvlog")
        self.assertEqual(row["fio_mib_s"], 500.0)

    def test_normalizes_common_result_and_ignores_audit_fields(self):
        payload = {
            "case": "e2e-nvme-rnvpce-append-write-bs-4k-r1",
            "device": "nvme",
            "mode": "rnvpce",
            "workload": "append-write",
            "axis": "bs",
            "value": "4k",
            "bs_kib": 4,
            "jobs": 1,
            "repeat": 1,
            "actual_mib": 2048,
            "fio_mib_s": 250.0,
            "including_sync_mib_s": 240.0,
            "iops": 64000.0,
            "mean_latency_us": 15.0,
            "p99_latency_us": 30.0,
            "fio_fsync_blocks": 1,
        }
        row = summarize.normalize_result(payload)
        self.assertEqual(list(row), list(summarize.RESULT_COLUMNS))
        self.assertNotIn("fio_fsync_blocks", row)


if __name__ == "__main__":
    unittest.main()
