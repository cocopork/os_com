#!/usr/bin/env python3

import json
import unittest

from results import parse_log, parse_profile


class VirtioForwarderResultsTest(unittest.TestCase):
    def test_profile_parser_keeps_latest_snapshot_per_queue(self):
        fields = (
            "queue=3 tsc_hz=2500000000 completions=65536 submitted_bios=65536 "
            "descriptor_wait_count=2 descriptor_wait_ticks=100 max_staging_depth=7 "
            "submit_samples=64 submit_id_alloc_ticks=1600 "
            "submit_dma_prepare_ticks=3200 submit_vq_acquire_ticks=4800 "
            "submit_vq_add_ticks=6400 submit_publish_notify_ticks=8000 "
            "submit_total_ticks=24000 completion_samples=64 "
            "completion_wait_ticks=64000 completion_pop_owner_ticks=1600 "
            "completion_descriptor_wake_ticks=3200 completion_response_id_ticks=4800 "
            "completion_bio_complete_ticks=6400 completion_total_ticks=16000"
        )
        body = f"[VIRTIO_PROFILE] {fields}\x1b[0m\n[VIRTIO_PROFILE] " + fields.replace(
            "completions=65536", "completions=131072"
        ).replace("submit_samples=64", "submit_samples=128").replace(
            "completion_samples=64", "completion_samples=128"
        )
        rows = parse_profile(body, "case")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["queue"], 3)
        self.assertEqual(rows[0]["completions"], 131072)
        self.assertEqual(rows[0]["submit_samples"], 128)
        self.assertEqual(rows[0]["submit_total_us_per_sample"], 0.075)

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
            f"VIRTIO_DIRECT_FIO_START repeat={repeat}\n{json.dumps(payload)}\n"
            f"VIRTIO_DIRECT_FIO_END repeat={repeat}"
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
            f"VIRTIO_DIRECT_FIO_START repeat={repeat}\n{json.dumps(payload)}\n"
            f"VIRTIO_DIRECT_FIO_END repeat={repeat}"
            for repeat in (1, 2, 3)
        )
        with self.assertRaisesRegex(ValueError, "short I/O"):
            parse_log(body, "case", 2048 * 1024 * 1024)


if __name__ == "__main__":
    unittest.main()
