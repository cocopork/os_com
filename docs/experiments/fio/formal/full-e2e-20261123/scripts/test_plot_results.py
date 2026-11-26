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
    def test_plot_mode_selection_includes_nvlog(self):
        selector = getattr(plot, "present_modes", None)
        self.assertIsNotNone(selector)
        rows = [{"mode": mode} for mode in ("ext4", "rnvpce", "nvlog")]
        self.assertEqual(selector(rows), ("ext4", "rnvpce", "nvlog"))

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

    def test_plot_all_keeps_only_two_bandwidth_summaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = []
            for device in ("nvme", "sata"):
                for workload in ("append-write", "overwrite-randwrite", "cold-randread"):
                    for axis in ("bs", "threads"):
                        for mode in ("ext4", "rnvpce", "nvlog"):
                            for repeat in (1, 2, 3):
                                rows.append({
                                    "device": device, "mode": mode, "workload": workload,
                                    "axis": axis, "value": "4k" if axis == "bs" else "4",
                                    "bs_kib": 4, "jobs": 1 if axis == "bs" else 4,
                                    "repeat": repeat, "fio_mib_s": 100 + repeat,
                                    "mean_latency_us": 40 + repeat,
                                    "p99_latency_us": 80 + repeat,
                                })
            with (root / "results.csv").open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=list(rows[0]))
                writer.writeheader()
                writer.writerows(rows)
            (root / "obsolete-subplot.png").touch()
            plot.plot_all(root / "results.csv", root)
            self.assertEqual(
                sorted(path.name for path in root.glob("*.png")),
                ["bandwidth-bs-summary.png", "bandwidth-thread-summary.png"],
            )

    def test_bs_bandwidth_summary_has_device_rows_and_workload_columns(self):
        rows = []
        for device in ("nvme", "sata"):
            for workload in ("append-write", "overwrite-randwrite", "cold-randread"):
                for mode in ("ext4", "rnvpce", "nvlog"):
                    for repeat in (1, 2, 3):
                        rows.append({
                            "device": device, "mode": mode, "workload": workload,
                            "axis": "bs", "value": "4k", "bs_kib": 4, "jobs": 1,
                            "repeat": repeat, "fio_mib_s": 100 + repeat,
                            "mean_latency_us": 40 + repeat, "p99_latency_us": 80 + repeat,
                        })

        builder = getattr(plot, "build_bs_bandwidth_summary", None)
        self.assertIsNotNone(builder)
        figure, axes = builder(rows)
        try:
            self.assertEqual(axes.shape, (2, 3))
            self.assertEqual(
                [axis.get_title() for axis in axes[0]],
                ["Append write", "Overwrite", "Random read"],
            )
            self.assertEqual(axes[0, 0].get_ylabel(), "NVMe\nBandwidth (MiB/s)")
            self.assertEqual(axes[1, 0].get_ylabel(), "SATA\nBandwidth (MiB/s)")
        finally:
            plot.plt.close(figure)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with (root / "results.csv").open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=list(rows[0]))
                writer.writeheader()
                writer.writerows(rows)
            plot.plot_all(root / "results.csv", root)
            self.assertTrue((root / "bandwidth-bs-summary.png").is_file())

    def test_thread_bandwidth_summary_has_device_rows_and_workload_columns(self):
        rows = []
        for device in ("nvme", "sata"):
            for workload in ("append-write", "overwrite-randwrite", "cold-randread"):
                for mode in ("ext4", "rnvpce", "nvlog"):
                    for repeat in (1, 2, 3):
                        rows.append({
                            "device": device, "mode": mode, "workload": workload,
                            "axis": "threads", "value": "4", "bs_kib": 4, "jobs": 4,
                            "repeat": repeat, "fio_mib_s": 100 + repeat,
                            "mean_latency_us": 40 + repeat, "p99_latency_us": 80 + repeat,
                        })

        builder = getattr(plot, "build_thread_bandwidth_summary", None)
        self.assertIsNotNone(builder)
        figure, axes = builder(rows)
        try:
            self.assertEqual(axes.shape, (2, 3))
            self.assertEqual(
                [axis.get_title() for axis in axes[0]],
                ["Append write", "Overwrite", "Random read"],
            )
            self.assertEqual([label.get_text() for label in axes[1, 0].get_xticklabels()], ["4"])
            self.assertEqual(axes[1, 0].get_xlabel(), "Threads")
        finally:
            plot.plt.close(figure)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with (root / "results.csv").open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=list(rows[0]))
                writer.writeheader()
                writer.writerows(rows)
            plot.plot_all(root / "results.csv", root)
            self.assertTrue((root / "bandwidth-thread-summary.png").is_file())


if __name__ == "__main__":
    unittest.main()
