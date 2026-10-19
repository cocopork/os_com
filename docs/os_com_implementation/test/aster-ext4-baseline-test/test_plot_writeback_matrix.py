#!/usr/bin/env python3

import csv
import importlib.util
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
PLOTTER = HERE / "plot_writeback_matrix.py"


def load_plotter():
    if not PLOTTER.is_file():
        raise AssertionError(f"plotter is missing: {PLOTTER}")
    spec = importlib.util.spec_from_file_location("plot_writeback_matrix", PLOTTER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_summary(path: Path, omit_last: bool = False) -> None:
    fieldnames = [
        "device",
        "fs",
        "workload",
        "mode",
        "njobs",
        "write_mib",
        "busy_ms",
        "drain_ms",
        "busy_bw_mib_s",
        "drain_bw_mib_s",
        "iops",
        "valid",
        "note",
    ]
    rows = []
    for device_index, device in enumerate(("sata", "nvme"), 1):
        for fs_index, filesystem in enumerate(
            (
                "ext2like_sync",
                "ext4_sync",
                "ext2like_nomerge",
                "ext4",
                "ext4_nomerge",
            ),
            1,
        ):
            for workload_index, workload in enumerate(
                ("randwrite-4k", "seqwrite-4k"), 1
            ):
                for njobs in (1, 4, 8, 12, 16, 20, 24, 28, 32):
                    busy = device_index * fs_index * workload_index * njobs
                    drain = busy + 100
                    rows.append(
                        {
                            "device": device,
                            "fs": filesystem,
                            "workload": workload,
                            "mode": "orderless",
                            "njobs": str(njobs),
                            "write_mib": "2048",
                            "busy_ms": "1000",
                            "drain_ms": "1100",
                            "busy_bw_mib_s": str(busy),
                            "drain_bw_mib_s": str(drain),
                            "iops": "1",
                            "valid": "1",
                            "note": "",
                        }
                    )
    if omit_last:
        rows.pop(0)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)


class WritebackMatrixPlotTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plotter = load_plotter()

    def test_loader_accepts_complete_matrix_and_uses_busy_bandwidth(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary)
            rows = self.plotter.load_summary(summary)

        self.assertEqual(len(rows), 180)
        self.assertEqual({row.mode for row in rows}, {"orderless"})
        self.assertEqual(
            {row.profile for row in rows},
            set(self.plotter.PROFILES),
        )
        first = next(
            row
            for row in rows
            if row.device == "sata"
            and row.profile == "ext2like_sync"
            and row.workload == "randwrite-4k"
            and row.njobs == 1
        )
        self.assertEqual(first.bandwidth_mib_s, 1.0)

    def test_loader_rejects_incomplete_matrix(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary, omit_last=True)
            with self.assertRaisesRegex(ValueError, "incomplete writeback matrix"):
                self.plotter.load_summary(summary)

    def test_figure_has_four_panels_five_profiles_and_unlabelled_device_limit(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary)
            rows = self.plotter.load_summary(summary)

        figure, axes = self.plotter.create_figure(rows, log_scale=False)
        try:
            self.assertEqual(axes.shape, (2, 2))
            self.assertIsNone(figure._suptitle)
            self.assertEqual(figure.legends[0]._ncols, 6)
            self.assertEqual(
                [text.get_text() for text in figure.legends[0].get_texts()],
                [
                    "EXT2-orderless",
                    "EXT4-orderless",
                    "rNVPC",
                    "rNVPCe",
                    "EXT4-no-merge",
                    "Ideal",
                ],
            )
            self.assertEqual(
                [axis.texts[-1].get_text() for axis in axes.flat],
                [
                    "(a) SATA random write",
                    "(b) SATA sequential write",
                    "(c) NVMe random write",
                    "(d) NVMe sequential write",
                ],
            )
            for axis in axes.flat:
                self.assertEqual(axis.get_xlabel(), "fio jobs")
                self.assertEqual(axis.get_yscale(), "linear")
                self.assertEqual(len(axis.get_lines()), 6)
                self.assertFalse(
                    any("MiB/s" in text.get_text() for text in axis.texts)
                )
                self.assertEqual(
                    [label.get_text() for label in axis.get_xticklabels()],
                    ["1", "4", "8", "12", "16", "20", "24", "28", "32"],
                )
                limit = axis.get_lines()[-1]
                self.assertEqual(limit.get_color(), "#C43C39")
                self.assertEqual(limit.get_linestyle(), "--")
        finally:
            self.plotter.plt.close(figure)

    def test_log_figure_uses_logarithmic_y_axes(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary)
            rows = self.plotter.load_summary(summary)

        figure, axes = self.plotter.create_figure(rows, log_scale=True)
        try:
            self.assertTrue(all(axis.get_yscale() == "log" for axis in axes.flat))
            self.assertTrue(all(axis.get_ylim()[0] > 0 for axis in axes.flat))
        finally:
            self.plotter.plt.close(figure)

    def test_plot_result_writes_linear_and_log_pdf_png(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "result"
            result.mkdir()
            write_summary(result / "summary.csv")
            outputs = self.plotter.plot_result(result)

            self.assertEqual(
                {path.name for path in outputs},
                {
                    "writeback-profiles-orderless-linear.pdf",
                    "writeback-profiles-orderless-linear.png",
                    "writeback-profiles-orderless-log.pdf",
                    "writeback-profiles-orderless-log.png",
                },
            )
            self.assertTrue(all(path.is_file() for path in outputs))

    def test_custom_summary_and_origin_suffix_do_not_replace_default_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "result"
            result.mkdir()
            summary = result / "summary copy.csv"
            write_summary(summary)

            outputs = self.plotter.plot_result(
                result,
                summary_path=summary,
                name_suffix="origin",
            )

            self.assertEqual(
                {path.name for path in outputs},
                {
                    "writeback-profiles-orderless-linear-origin.pdf",
                    "writeback-profiles-orderless-linear-origin.png",
                    "writeback-profiles-orderless-log-origin.pdf",
                    "writeback-profiles-orderless-log-origin.png",
                },
            )
            self.assertFalse(
                (result / "writeback-profiles-orderless-linear.png").exists()
            )

    def test_cli_accepts_custom_summary_and_name_suffix(self):
        args = self.plotter.parse_args(
            [
                "--summary",
                "/tmp/summary copy.csv",
                "--name-suffix",
                "origin",
            ]
        )
        self.assertEqual(args.summary, Path("/tmp/summary copy.csv"))
        self.assertEqual(args.name_suffix, "origin")


if __name__ == "__main__":
    unittest.main()
