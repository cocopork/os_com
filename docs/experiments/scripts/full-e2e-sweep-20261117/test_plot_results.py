# SPDX-License-Identifier: MPL-2.0
import csv
import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("plot_results.py")
SPEC = importlib.util.spec_from_file_location("full_e2e_plot", SCRIPT)
plot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plot)


class PlotTests(unittest.TestCase):
    def test_axis_ticks_match_every_test_point(self):
        figure, axes = plot.plt.subplots(1, 2)
        try:
            plot.configure_x_axis(axes[0], "threads", [1, 4, 8, 12, 16, 20, 24, 28, 32])
            self.assertEqual(
                list(axes[0].get_xticks()),
                [1, 4, 8, 12, 16, 20, 24, 28, 32],
            )

            plot.configure_x_axis(axes[1], "bs", [4, 8, 16, 32, 64, 128, 256, 512, 1024])
            self.assertEqual(
                list(axes[1].get_xticks()),
                [4, 8, 16, 32, 64, 128, 256, 512, 1024],
            )
            self.assertEqual(
                [label.get_text() for label in axes[1].get_xticklabels()],
                ["4K", "8K", "16K", "32K", "64K", "128K", "256K", "512K", "1024K"],
            )
        finally:
            plot.plt.close(figure)

    def test_plot_creates_bandwidth_and_latency_figures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = []
            for device in ("nvme", "sata"):
                for mode in ("ext4", "rnvpc", "rnvpce"):
                    for repeat in (1, 2, 3):
                        rows.append({
                            "device": device, "mode": mode, "workload": "overwrite-randwrite",
                            "axis": "threads", "value": "4", "bs_kib": 4, "jobs": 4,
                            "repeat": repeat, "fio_mib_s": 100 + repeat,
                            "mean_latency_us": 40 + repeat, "p99_latency_us": 80 + repeat,
                        })
            with (root / "results.csv").open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=list(rows[0]))
                writer.writeheader()
                writer.writerows(rows)
            plot.plot_all(root / "results.csv", root)
            self.assertTrue((root / "overwrite-randwrite-threads-bandwidth.png").is_file())
            self.assertTrue((root / "overwrite-randwrite-threads-mean-latency.png").is_file())
            self.assertTrue((root / "overwrite-randwrite-threads-p99-latency.png").is_file())


if __name__ == "__main__":
    unittest.main()
