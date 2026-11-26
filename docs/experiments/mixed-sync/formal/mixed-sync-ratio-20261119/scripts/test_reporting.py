# SPDX-License-Identifier: MPL-2.0

import csv
import importlib
import json
from pathlib import Path
import subprocess
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
        "latency_overflow_count": 0,
        "mean_latency_us": 1000000 / sample,
        "p50_latency_us": 500000 / sample,
        "p95_latency_us": 1500000 / sample,
        "p99_latency_us": 2000000 / sample,
        "p999_latency_us": 3000000 / sample,
    }


def synthetic_nvlog_rows() -> list[dict]:
    rows = []
    for point in matrix.matrix():
        if point.mode != "rnvpce":
            continue
        row = synthetic_row(point)
        row["case"] = row["case"].replace("-rnvpce-", "-nvlog-")
        row["mode"] = "nvlog"
        row["cache_control"] = "memcg-approx"
        rows.append(row)
    return rows


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

    def test_aggregate_accepts_complete_four_system_archive(self) -> None:
        rows = self.rows + synthetic_nvlog_rows()
        try:
            aggregated = summarize.aggregate(
                rows, modes=("ext2", "ext4", "rnvpce", "nvlog")
            )
        except (TypeError, ValueError):
            aggregated = []
        self.assertEqual(len(aggregated), 192)

    def test_normalizes_nvlog_payload_to_common_result_contract(self) -> None:
        payload = {
            "benchmark_exit": 0,
            "sync_exit": 0,
            "unmount_exit": 0,
            "qemu_exit": 0,
            "e2fsck_exit": 0,
            "parse_valid": True,
            "case": "mixed-nvme-nvlog-r30w70-sync40-r2",
            "device": "nvme",
            "mode": "nvlog",
            "read_percent": 30,
            "write_percent": 70,
            "sync_percent": 40,
            "repeat": 2,
            "threads": 8,
            "file_bytes": 335544320,
            "files_opened": 8,
            "configured_dram_cache_mib": 128,
            "configured_nv_data_cache_mib": 128,
            "configured_total_data_cache_mib": 256,
            "cache_control": "memcg-approx",
            "operations": 3000,
            "reads": 900,
            "writes": 2100,
            "fsyncs": 840,
            "elapsed_seconds": 30.0,
            "operations_per_second": 100.0,
            "mib_per_second": 0.390625,
            "latency_overflow_count": 0,
            "mean_latency_us": 80.0,
            "p50_latency_us": 50,
            "p95_latency_us": 150,
            "p99_latency_us": 200,
            "p999_latency_us": 300,
        }
        normalizer = getattr(summarize, "normalize_nvlog_result", None)
        self.assertIsNotNone(normalizer)
        row = normalizer(payload)
        self.assertEqual(row["prewrite_bytes"], 2684354560)
        self.assertEqual(row["file_mib"], 320)
        self.assertEqual(row["dataset_mib"], 2560)
        self.assertEqual(row["actual_read_percent"], 30.0)
        self.assertEqual(row["actual_sync_percent"], 40.0)

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

    def test_plot_includes_nvlog_when_integrated_rows_are_present(self) -> None:
        rows = self.rows + synthetic_nvlog_rows()
        figure, axis = plot_results.build_figure(
            rows, "nvme", 30, "operations_per_second"
        )
        try:
            figure.canvas.draw()
            labels = axis.get_legend_handles_labels()[1]
            self.assertIn("NVLog", labels)
            self.assertEqual(len(axis.lines), 4)
        finally:
            import matplotlib.pyplot as plt

            plt.close(figure)

    def test_summary_figure_has_two_device_rows_and_four_mix_columns(self) -> None:
        builder = getattr(plot_results, "build_summary_figure", None)
        self.assertIsNotNone(builder)
        rows = self.rows + synthetic_nvlog_rows()
        figure, axes = builder(rows, "mib_per_second")
        try:
            figure.canvas.draw()
            self.assertEqual(axes.shape, (2, 4))
            self.assertEqual(
                [axis.get_title() for axis in axes[0]],
                ["R:W = 0:10", "R:W = 3:7", "R:W = 5:5", "R:W = 7:3"],
            )
            self.assertEqual([axis.get_ylabel() for axis in axes[:, 0]], ["NVMe\nMiB/s", "SATA\nMiB/s"])
            self.assertTrue(all(len(axis.lines) == 4 for axis in axes.flat))
        finally:
            import matplotlib.pyplot as plt

            plt.close(figure)

    def test_writes_plotting_csv_and_only_three_summary_images(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            summarize.write_csv(output / "results.csv", self.rows)
            plot_results.generate(self.rows, output)
            images = sorted(output.glob("*.png"))
            image_names = {image.name for image in images}
            with (output / "plotting-source.csv").open() as source:
                plotting_rows = list(csv.DictReader(source))

        self.assertEqual(len(images), 3)
        self.assertIn("mixed-sync-bandwidth-summary.png", image_names)
        self.assertIn("mixed-sync-iops-summary.png", image_names)
        self.assertIn("mixed-sync-p99-latency-summary.png", image_names)
        self.assertEqual(len(plotting_rows), 432)

    def test_integration_cli_merges_complete_nvlog_matrix(self) -> None:
        nvlog_rows = synthetic_nvlog_rows()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base_csv = root / "base-results.csv"
            nvlog_root = root / "nvlog"
            output = root / "output"
            summarize.write_csv(base_csv, self.rows)
            for row in nvlog_rows:
                point = nvlog_root / row["case"]
                point.mkdir(parents=True)
                payload = dict(row)
                payload.update(
                    {
                        "benchmark_exit": 0,
                        "sync_exit": 0,
                        "unmount_exit": 0,
                        "qemu_exit": 0,
                        "e2fsck_exit": 0,
                        "parse_valid": True,
                        "file_bytes": row["file_mib"] * 1024 * 1024,
                        "files_opened": row["threads"],
                        "latency_overflow_count": 0,
                    }
                )
                (point / "result.json").write_text(json.dumps(payload))

            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT_DIR / "integrate_nvlog.py"),
                    str(base_csv),
                    str(nvlog_root),
                    str(output),
                    "--skip-plots",
                ],
                text=True,
                capture_output=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            repeated = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT_DIR / "integrate_nvlog.py"),
                    str(output / "results.csv"),
                    str(nvlog_root),
                    str(output),
                    "--skip-plots",
                ],
                text=True,
                capture_output=True,
            )
            self.assertEqual(repeated.returncode, 0, repeated.stderr)
            with (output / "results.csv").open(newline="") as source:
                combined = list(csv.DictReader(source))
            with (output / "aggregate.csv").open(newline="") as source:
                aggregated = list(csv.DictReader(source))
            with (output / "nvlog-results.csv").open(newline="") as source:
                nvlog = list(csv.DictReader(source))

        self.assertEqual(len(combined), 576)
        self.assertEqual(len(aggregated), 192)
        self.assertEqual(len(nvlog), 144)


if __name__ == "__main__":
    unittest.main()
