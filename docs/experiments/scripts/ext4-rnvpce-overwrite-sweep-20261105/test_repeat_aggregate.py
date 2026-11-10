#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import unittest

import repeat_aggregate


class RepeatAggregateTests(unittest.TestCase):
    def test_merge_preserves_existing_aggregate_metadata_for_unrelated_cases(self):
        baseline = [
            {
                "case": "existing-bs",
                "fio_mib_s": "500",
                "sample_count": "3",
                "selected_round": "2",
                "statistic": "median_fio_sample",
            },
            {"case": "new-thread", "fio_mib_s": "100", "sample_count": "1"},
        ]
        selected = {
            "new-thread": {
                "case": "new-thread",
                "fio_mib_s": "200",
                "sample_count": "3",
                "selected_round": "1",
                "statistic": "median_fio_sample",
            }
        }

        merged = repeat_aggregate.merge_selected(baseline, selected)

        self.assertEqual(merged[0]["sample_count"], "3")
        self.assertEqual(merged[0]["selected_round"], "2")
        self.assertEqual(merged[1]["fio_mib_s"], "200")

    def test_selects_complete_sample_with_median_fio_bandwidth(self):
        rows = [
            {"case": "point", "fio_mib_s": "900", "including_sync_mib_s": "600", "round": "1"},
            {"case": "point", "fio_mib_s": "300", "including_sync_mib_s": "290", "round": "2"},
            {"case": "point", "fio_mib_s": "500", "including_sync_mib_s": "450", "round": "3"},
        ]

        selected = repeat_aggregate.median_samples(rows, expected_rounds=3)

        self.assertEqual(selected["point"]["fio_mib_s"], "500")
        self.assertEqual(selected["point"]["including_sync_mib_s"], "450")
        self.assertEqual(selected["point"]["selected_round"], "3")

    def test_rejects_missing_repeat(self):
        rows = [
            {"case": "point", "fio_mib_s": "300", "round": "1"},
            {"case": "point", "fio_mib_s": "500", "round": "2"},
        ]
        with self.assertRaisesRegex(ValueError, "expected 3 repeats"):
            repeat_aggregate.median_samples(rows, expected_rounds=3)


if __name__ == "__main__":
    unittest.main()
