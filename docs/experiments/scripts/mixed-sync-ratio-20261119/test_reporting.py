# SPDX-License-Identifier: MPL-2.0

import csv
import importlib
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import matrix
import plot_results
import summarize


def synthetic_row(point: matrix.Point) -> dict:
    base = 1000 + point.read_percent * 10 - point.sync_percent * 2
    sample = base + point.repeat * 10
    return {
        "case": point.case,
        "device": point.device,
        "mode": point.mode,
        "read_percent": point.read_percent,
        "write_percent": 100 - point.read_percent,
        "sync_percent": point.sync_percent,
        "repeat": point.repeat,
        "threads": 8,
        "file_mib": 320,
        "dataset_mib": 2560,
        "configured_dram_cache_mib": point.dram_cache_mib,
        "configured_nv_data_cache_mib": point.nv_data_cache_mib,
        "configured_total_data_cache_mib": 256,
        "cache_control": point.cache_control,
        "prewrite_bytes": 8 * 335544320,
        "operations": sample * 30,
        "reads": sample * 9,
        "writes": sample * 21,
        "fsyncs": round(sample * 21 * point.sync_percent / 100),
        "actual_read_percent": 30.0,
        "actual_sync_percent": float(point.sync_percent),
        "elapsed_seconds": 30.0,
        "operations_per_second": float(sample),
        "mib_per_second": sample / 256,
        "mean_latency_us": 1000000 / sample,
        "p50_latency_us": 500000 / sample,
        "p95_latency_us": 1500000 / sample,
        "p99_latency_us": 2000000 / sample,
        "p999_latency_us": 3000000 / sample,
    }


class ReportingTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rows = [synthetic_row(point) for point in matrix.matrix()]

    def test_aggregate_uses_three_samples_and_population_statistics(self) -> None:
        aggregated = summarize.aggregate(self.rows)
        target = next(
            row
            for row in aggregated
            if row["device"] == "nvme"
            and row["mode"] == "ext4"
            and row["read_percent"] == 30
            and row["sync_percent"] == 40
        )
        self.assertEqual(len(aggregated), 144)
        self.assertEqual(target["samples"], 3)
        self.assertEqual(target["operations_per_second_mean"], 1240.0)
        self.assertAlmostEqual(target["operations_per_second_stddev"], 8.1649658093)
        self.assertEqual(target["operations_per_second_min"], 1230.0)
        self.assertEqual(target["operations_per_second_max"], 1250.0)

    def test_complete_matrix_validation_rejects_a_missing_round(self) -> None:
        summarize.validate_complete(self.rows)
        with self.assertRaisesRegex(ValueError, "missing"):
            summarize.validate_complete(self.rows[:-1])

    def test_load_results_revalidates_saved_result_against_raw_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for point in matrix.matrix():
                point_dir = root / point.case
                point_dir.mkdir()
                (point_dir / "result.json").write_text("{}\n")
            with self.assertRaisesRegex(ValueError, "saved result"):
                summarize.load_results(root)

    def test_plot_has_categorical_sync_ticks_filesystems_and_raw_samples(self) -> None:
        figure, axis = plot_results.build_figure(
            self.rows, "nvme", 30, "operations_per_second"
        )
        try:
            figure.canvas.draw()
            self.assertEqual(
                [label.get_text() for label in axis.get_xticklabels()],
                ["0%", "20%", "40%", "60%", "80%", "100%"],
            )
            labels = axis.get_legend_handles_labels()[1]
            self.assertIn("EXT2", labels)
            self.assertIn("EXT4", labels)
            self.assertIn("rNVPCe-SU", labels)
            self.assertIn("individual rounds", labels)
        finally:
            import matplotlib.pyplot as plt

            plt.close(figure)

    def test_writes_plotting_csv_and_sixteen_images(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            summarize.write_csv(output / "results.csv", self.rows)
            plot_results.generate(self.rows, output)
            images = sorted(output.glob("*.png"))
            with (output / "plotting-source.csv").open() as source:
                plotting_rows = list(csv.DictReader(source))

        self.assertEqual(len(images), 16)
        self.assertEqual(len(plotting_rows), 432)


if __name__ == "__main__":
    unittest.main()
