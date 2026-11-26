#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import unittest

import mean_aggregate


class MeanAggregateTests(unittest.TestCase):
    def test_averages_five_samples_and_preserves_identity(self):
        rows = []
        for round_number, fio in enumerate((100, 200, 300, 400, 500), start=1):
            rows.append(
                {
                    "case": "overwrite-rnvpce-bs-write-128k",
                    "mode": "rnvpce",
                    "axis": "bs",
                    "workload": "write",
                    "value": "128k",
                    "bs_kib": "128",
                    "jobs": "1",
                    "actual_mib": "2048",
                    "fio_mib_s": str(fio),
                    "including_sync_mib_s": str(fio - 10),
                    "iops": str(fio * 8),
                    "mean_latency_us": str(1000 / fio),
                    "p99_latency_us": str(2000 / fio),
                    "round": str(round_number),
                }
            )

        averaged = mean_aggregate.average_samples(rows, expected_rounds=5)

        row = averaged["overwrite-rnvpce-bs-write-128k"]
        self.assertEqual(row["mode"], "rnvpce")
        self.assertEqual(float(row["fio_mib_s"]), 300.0)
        self.assertEqual(float(row["including_sync_mib_s"]), 290.0)
        self.assertEqual(row["sample_count"], "5")
        self.assertEqual(row["statistic"], "arithmetic_mean")

    def test_rejects_a_case_with_missing_round(self):
        rows = [
            {"case": "point", "fio_mib_s": str(value), "round": str(value)}
            for value in range(4)
        ]
        with self.assertRaisesRegex(ValueError, "expected 5 repeats"):
            mean_aggregate.average_samples(rows, expected_rounds=5)


if __name__ == "__main__":
    unittest.main()
