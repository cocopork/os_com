#!/usr/bin/env python3
import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("latency_nvpc.py")


class NvpcLatencyParserTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        spec = importlib.util.spec_from_file_location("latency_nvpc", MODULE_PATH)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_completion_percentages_exclude_overlapping_device_latency(self) -> None:
        stats = {
            "nvpc_flush_materialize": (2, 100, 120),
            "nvpc_flush_durability": (1, 300, 300),
            "nvpc_flush_migrate": (2, 50, 60),
            "nvpc_flush_reclaim": (2, 50, 55),
            "nvpc_flush_device": (2, 1000, 1100),
        }

        rows = self.module.phase_rows(stats, self.module.COMPLETION_FIELDS)

        self.assertEqual([row[0] for row in rows], self.module.COMPLETION_FIELDS)
        self.assertAlmostEqual(sum(row[5] for row in rows), 100.0)
        self.assertAlmostEqual(rows[0][5], 200 / 700 * 100)
        self.assertNotIn("nvpc_flush_device", self.module.COMPLETION_FIELDS)
        self.assertIn("nvpc_flush_device", self.module.DEVICE_FIELDS)


if __name__ == "__main__":
    unittest.main()
