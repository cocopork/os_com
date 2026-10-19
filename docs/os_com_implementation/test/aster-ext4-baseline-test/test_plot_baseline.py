#!/usr/bin/env python3

import csv
import importlib.util
import math
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
PLOTTER = HERE / "plot_baseline.py"


def load_plotter():
    if not PLOTTER.is_file():
        raise AssertionError(f"plotter is missing: {PLOTTER}")
    spec = importlib.util.spec_from_file_location("plot_baseline", PLOTTER)
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
        for fs_index, filesystem in enumerate(("ext2like", "ext4"), 1):
            for workload_index, workload in enumerate(
                ("randwrite-4k", "seqwrite-4k"), 1
            ):
                for mode_index, mode in enumerate(("fsync", "orderless"), 1):
                    for njobs in (1, 4, 8, 12, 16, 20, 24, 28, 32):
                        busy = device_index * fs_index * workload_index * mode_index * njobs
                        drain = busy + 1000
                        valid = not (
                            device == "nvme"
                            and filesystem == "ext4"
                            and workload == "seqwrite-4k"
                            and mode == "fsync"
                            and njobs == 32
                        )
                        rows.append(
                            {
                                "device": device,
                                "fs": filesystem,
                                "workload": workload,
                                "mode": mode,
                                "njobs": str(njobs),
                                "write_mib": "2048",
                                "busy_ms": "1000",
                                "drain_ms": "1100",
                                "busy_bw_mib_s": str(busy),
                                "drain_bw_mib_s": str(drain),
                                "iops": "1",
                                "valid": "1" if valid else "0",
                                "note": "" if valid else "incomplete write",
                            }
                        )
    if omit_last:
        rows.pop()
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)


class AsterBaselinePlotTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plotter = load_plotter()

    def test_loader_selects_mode_metric_and_keeps_invalid_as_gap(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary)
            rows = self.plotter.load_summary(summary)

        fsync = next(
            row
            for row in rows
            if row.device == "sata"
            and row.profile == "ext2like-fsync"
            and row.workload == "randwrite-4k"
            and row.njobs == 1
        )
        orderless = next(
            row
            for row in rows
            if row.device == "sata"
            and row.profile == "ext2like-orderless"
            and row.workload == "randwrite-4k"
            and row.njobs == 1
        )
        invalid = next(row for row in rows if not row.valid)

        self.assertEqual(fsync.bandwidth_mib_s, 1.0)
        self.assertEqual(orderless.bandwidth_mib_s, 1002.0)
        self.assertTrue(math.isnan(invalid.bandwidth_mib_s))
        self.assertEqual(len(rows), 144)

    def test_default_result_uses_summary_snapshot_in_plot_directory(self):
        expected = (
            self.plotter.HERE
            / "plots"
            / "20260921_180514_aster-ext4-baseline"
        )

        self.assertEqual(self.plotter.DEFAULT_RESULT, expected)
        self.assertEqual(
            self.plotter.parse_args([]).result_dir / "summary.csv",
            expected / "summary.csv",
        )

    def test_loader_rejects_incomplete_matrix(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary, omit_last=True)
            with self.assertRaisesRegex(ValueError, "incomplete baseline matrix"):
                self.plotter.load_summary(summary)

    def test_figure_is_two_by_two_with_four_profiles_and_panel_limit(self):
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / "summary.csv"
            write_summary(summary)
            rows = self.plotter.load_summary(summary)

        figure, axes = self.plotter.create_figure(rows)
        try:
            self.assertEqual(axes.shape, (2, 2))
            self.assertEqual(
                [axis.texts[-1].get_text() for axis in axes.flat],
                [
                    "(a) SATA random write",
                    "(b) SATA sequential write",
                    "(c) NVMe random write",
                    "(d) NVMe sequential write",
                ],
            )
            expected_limits = (1128.0, 1256.0, 1256.0, 1512.0)
            for axis, expected_limit in zip(axes.flat, expected_limits):
                self.assertEqual(axis.get_xlabel(), "fio jobs")
                self.assertEqual(len(axis.get_lines()), 5)
                self.assertEqual(
                    [label.get_text() for label in axis.get_xticklabels()],
                    ["1", "4", "8", "12", "16", "20", "24", "28", "32"],
                )
                limit_line = axis.get_lines()[4]
                self.assertEqual(limit_line.get_color(), "#C62828")
                self.assertEqual(limit_line.get_linestyle(), "--")
                self.assertEqual(list(limit_line.get_ydata()), [expected_limit] * 2)
                self.assertIn(
                    f"Device limit: {expected_limit:,.0f} MiB/s",
                    [text.get_text() for text in axis.texts],
                )
            self.assertEqual(figure.legends[0]._ncols, 4)
            invalid_line = axes[1, 1].get_lines()[2]
            self.assertTrue(math.isnan(invalid_line.get_ydata()[-1]))
        finally:
            self.plotter.plt.close(figure)

    def test_plot_result_writes_pdf_and_png_to_matching_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = root / "20260921_180514_aster-ext4-baseline"
            output = root / "plots" / result.name
            result.mkdir()
            write_summary(result / "summary.csv")

            outputs = self.plotter.plot_result(result, output)

            expected_dir = self.plotter.HERE / "plots" / result.name
            self.assertEqual(self.plotter.default_output_dir(result), expected_dir)
            self.assertEqual({path.parent for path in outputs}, {output})
            self.assertEqual(
                {path.name for path in outputs},
                {
                    "asterinas-ext2like-ext4-sync-orderless.pdf",
                    "asterinas-ext2like-ext4-sync-orderless.png",
                },
            )
            self.assertTrue(all(path.is_file() for path in outputs))


if __name__ == "__main__":
    unittest.main()
