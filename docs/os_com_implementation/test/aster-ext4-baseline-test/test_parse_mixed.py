#!/usr/bin/env python3
import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("parse_mixed.py")


class MixedParserTest(unittest.TestCase):
    def test_reports_read_write_and_total_drain_bandwidth(self):
        spec = importlib.util.spec_from_file_location("parse_mixed", MODULE_PATH)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        log = """
=== FIO_RESULT: nvme_rnvpc_extend_randrw-r25-4k_orderless_nj8 ===
{"jobs":[{"read":{"io_bytes":67108864,"bw_bytes":33554432,"iops":8192},
          "write":{"io_bytes":201326592,"bw_bytes":100663296,"iops":24576}}]}
=== END_FIO_RESULT ===
=== DRAIN: busy_ns=2000000000 drain_ns=4000000000 ===
=== ASTER_BASELINE_COMPLETE ===
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run.log"
            path.write_text(log)
            rows = module.parse_log(path)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertAlmostEqual(row["read_ratio_pct"], 25.0)
        self.assertAlmostEqual(row["busy_total_bw_mib_s"], 128.0)
        self.assertAlmostEqual(row["drain_total_bw_mib_s"], 64.0)
        self.assertAlmostEqual(row["drain_read_bw_mib_s"], 16.0)
        self.assertAlmostEqual(row["drain_write_bw_mib_s"], 48.0)


if __name__ == "__main__":
    unittest.main()
