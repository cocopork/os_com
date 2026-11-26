#!/usr/bin/env python3

import json
import unittest

from results import parse_log


class VirtioForwarderResultsTest(unittest.TestCase):
    def test_parser_extracts_three_complete_randwrite_runs(self):
        payload = {
            "jobs": [{"write": {
                "io_bytes": 2048 * 1024 * 1024,
                "bw_bytes": 200 * 1024 * 1024,
                "iops": 51200.0,
                "lat_ns": {"mean": 1500.0},
                "clat_ns": {"mean": 1000.0,
                            "percentile": {"99.000000": 2000.0}},
            }}]
        }
        body = "\n".join(
            f"VIRTIO_POLL_FIO_START repeat={repeat}\n{json.dumps(payload)}\n"
            f"VIRTIO_POLL_FIO_END repeat={repeat}"
            for repeat in (1, 2, 3)
        )
        rows = parse_log(body, "case", 2048 * 1024 * 1024)
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]["fio_mib_s"], 200.0)
        self.assertEqual(rows[0]["p99_latency_us"], 2.0)

    def test_parser_rejects_short_io(self):
        payload = {"jobs": [{"write": {
            "io_bytes": 1, "bw_bytes": 1, "iops": 1,
            "lat_ns": {"mean": 2},
            "clat_ns": {"mean": 1, "percentile": {"99.000000": 1}},
        }}]}
        body = "\n".join(
            f"VIRTIO_POLL_FIO_START repeat={repeat}\n{json.dumps(payload)}\n"
            f"VIRTIO_POLL_FIO_END repeat={repeat}"
            for repeat in (1, 2, 3)
        )
        with self.assertRaisesRegex(ValueError, "short I/O"):
            parse_log(body, "case", 2048 * 1024 * 1024)


if __name__ == "__main__":
    unittest.main()
