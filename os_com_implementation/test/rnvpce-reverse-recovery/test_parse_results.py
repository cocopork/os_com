import csv
import subprocess
import tempfile
import unittest
from pathlib import Path


class ParseResultsTest(unittest.TestCase):
    def test_groups_samples_and_converts_nanoseconds_to_milliseconds(self):
        marker = (
            "[RNVPC_RECOVERY_PROFILE] total_ns={total} descriptor_scan_ns=10 "
            "runtime_rebuild_ns=20 metadata_discovery_ns=30 pending_repair_ns=40 "
            "extent_scan_ns=50 reverse_build_ns=60 allocation_scan_ns=70 "
            "reclaim_ns=80 data_index_restore_ns=90 inode_roots=1 extent_nodes=2 "
            "extent_entries=3 reverse_entries=4 pool_total=100 pool_free=5 "
            "pool_meta=20 pool_data=75"
        )
        log = "\n".join(
            [
                "=== RNVPC_RECOVERY_RUN_BEGIN target_files=1 created_files=1 data_bytes=4096 fill_status=0 repeat=1 ===",
                marker.format(total=1_000_000),
                "=== RNVPC_RECOVERY_RUN_BEGIN target_files=1 created_files=1 data_bytes=4096 fill_status=0 repeat=2 ===",
                marker.format(total=3_000_000),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            source = directory / "serial.log"
            raw = directory / "raw.csv"
            summary = directory / "summary.csv"
            source.write_text(log)
            subprocess.run(
                [
                    "python3",
                    str(Path(__file__).with_name("parse_results.py")),
                    str(source),
                    str(raw),
                    str(summary),
                ],
                check=True,
            )
            with summary.open(newline="") as stream:
                row = next(csv.DictReader(stream))
            self.assertEqual(row["samples"], "2")
            self.assertEqual(row["fill_status"], "0")
            self.assertEqual(row["total_median_ms"], "2.0")
            self.assertEqual(row["reverse_entries"], "4")


if __name__ == "__main__":
    unittest.main()
