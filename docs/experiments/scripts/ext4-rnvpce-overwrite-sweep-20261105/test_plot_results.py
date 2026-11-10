#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

import csv
from pathlib import Path
import tempfile
import unittest

import plot_results


FIELDS = (
    "case",
    "mode",
    "axis",
    "workload",
    "value",
    "bs_kib",
    "jobs",
    "actual_mib",
    "fio_mib_s",
    "including_sync_mib_s",
    "iops",
    "mean_latency_us",
    "p99_latency_us",
)


class PlotResultsTests(unittest.TestCase):
    def test_legend_only_describes_filesystems_and_optional_repeat_samples(self):
        self.assertEqual(
            [handle.get_label() for handle in plot_results._legend_handles(False)],
            ["EXT4 writeback", "rNVPCe (4 workers)"],
        )
        self.assertNotIn(
            "including sync",
            [handle.get_label() for handle in plot_results._legend_handles(True)],
        )

    def test_repeat_paths_accept_one_or_multiple_csv_files(self):
        one = Path("one.csv")
        two = Path("two.csv")
        self.assertEqual(plot_results._repeat_paths(one), [one])
        self.assertEqual(plot_results._repeat_paths([one, two]), [one, two])

    def test_repeat_legend_describes_samples_without_calling_them_anomalies(self):
        self.assertEqual(plot_results._legend_handles(True)[-1].get_label(), "repeat samples (fio)")

    def test_repeat_samples_expand_y_axis_instead_of_being_clipped(self):
        with tempfile.TemporaryDirectory() as directory:
            csv_path = Path(directory) / "repeats.csv"
            with csv_path.open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=FIELDS)
                writer.writeheader()
                writer.writerow({
                    "case": "repeat",
                    "mode": "ext4",
                    "axis": "threads",
                    "workload": "randwrite",
                    "value": "1",
                    "bs_kib": "4",
                    "jobs": "1",
                    "actual_mib": "2048",
                    "fio_mib_s": "200",
                    "including_sync_mib_s": "180",
                    "iops": "1",
                    "mean_latency_us": "1",
                    "p99_latency_us": "1",
                })
            figure, axes = plot_results.plt.subplots(1, 2)
            for axis in axes:
                axis.set_ylim(0, 100)

            plot_results._overlay_repeats(axes, csv_path, "threads")

            self.assertGreater(axes[0].get_ylim()[1], 200)
            plot_results.plt.close(figure)

    def test_load_matrix_sorts_numeric_axes(self):
        with tempfile.TemporaryDirectory() as directory:
            csv_path = Path(directory) / "results.csv"
            rows = []
            for mode in ("ext4", "rnvpce"):
                for workload in ("randwrite", "write"):
                    for jobs in (4, 1):
                        rows.append(
                            {
                                "case": f"{mode}-{workload}-{jobs}",
                                "mode": mode,
                                "axis": "threads",
                                "workload": workload,
                                "value": str(jobs),
                                "bs_kib": "4",
                                "jobs": str(jobs),
                                "actual_mib": "2048",
                                "fio_mib_s": str(jobs * 100),
                                "including_sync_mib_s": str(jobs * 90),
                                "iops": "1",
                                "mean_latency_us": "1",
                                "p99_latency_us": "1",
                            }
                        )
                    for bs_kib in (128, 4):
                        rows.append(
                            {
                                "case": f"{mode}-{workload}-{bs_kib}",
                                "mode": mode,
                                "axis": "bs",
                                "workload": workload,
                                "value": f"{bs_kib}k",
                                "bs_kib": str(bs_kib),
                                "jobs": "1",
                                "actual_mib": "2048",
                                "fio_mib_s": str(bs_kib * 10),
                                "including_sync_mib_s": str(bs_kib * 9),
                                "iops": "1",
                                "mean_latency_us": "1",
                                "p99_latency_us": "1",
                            }
                        )
            with csv_path.open("w", newline="") as output:
                writer = csv.DictWriter(output, fieldnames=FIELDS)
                writer.writeheader()
                writer.writerows(rows)

            matrix = plot_results.load_matrix(csv_path)

            self.assertEqual([row.x for row in matrix["threads"]["randwrite"]["ext4"]], [1, 4])
            self.assertEqual([row.x for row in matrix["bs"]["write"]["rnvpce"]], [4, 128])

    def test_render_creates_thread_and_block_size_pngs(self):
        source = Path(
            "/home/zn/asterinas-build-env/"
            "ext4-rnvpce-overwrite-sweep-20261105/results/results.csv"
        )
        if not source.exists():
            self.skipTest("validated experiment CSV is unavailable")
        with tempfile.TemporaryDirectory() as directory:
            outputs = plot_results.render(source, Path(directory))

            self.assertEqual({path.name for path in outputs}, {
                "ext4-rnvpce-4worker-thread-sweep.png",
                "ext4-rnvpce-4worker-bs-sweep.png",
            })
            self.assertTrue(all(path.stat().st_size > 50_000 for path in outputs))


if __name__ == "__main__":
    unittest.main()
