#!/usr/bin/env python3

import tempfile
import unittest
from pathlib import Path

import matplotlib.pyplot as plt

from plot_results import build_bandwidth_summary, load_rows


class RawDevicePlotTest(unittest.TestCase):
    def setUp(self):
        self.rows = [
            {
                "device": device,
                "system": system,
                "workload": workload,
                "axis": axis,
                "bs_kib": 4,
                "jobs": 1,
                "repeat": repeat,
                "fio_mib_s": 100.0 + repeat,
            }
            for device in ("nvme", "sata")
            for system in ("asterinas", "linux")
            for workload in ("append-write", "overwrite-randwrite", "cold-randread")
            for axis in ("bs", "threads")
            for repeat in (1, 2, 3)
        ]

    def test_summary_uses_two_by_three_layout(self):
        figure, axes = build_bandwidth_summary(self.rows, "bs")
        self.assertEqual(axes.shape, (2, 3))
        self.assertEqual([axis.get_title() for axis in axes[0]],
                         ["Sequential write", "Random write", "Random read"])
        self.assertEqual(axes[0, 0].get_ylabel(), "NVMe\nBandwidth (MiB/s)")
        self.assertEqual(axes[1, 0].get_ylabel(), "SATA\nBandwidth (MiB/s)")
        plt.close(figure)

    def test_loader_reads_repetition_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            csv_path = Path(directory) / "results.csv"
            csv_path.write_text(
                "device,system,workload,axis,bs_kib,jobs,repeat,fio_mib_s\n"
                "nvme,asterinas,cold-randread,bs,4,1,1,31.1\n"
            )
            rows = load_rows(csv_path)
        self.assertEqual(rows[0]["bs_kib"], 4)
        self.assertEqual(rows[0]["fio_mib_s"], 31.1)


if __name__ == "__main__":
    unittest.main()
