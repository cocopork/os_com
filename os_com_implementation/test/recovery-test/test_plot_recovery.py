from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import plot_recovery


HERE = Path(__file__).resolve().parent


class PlotRecoveryTest(unittest.TestCase):
    def test_uses_paper_facing_capacity_and_phase_labels(self) -> None:
        self.assertEqual(
            [plot_recovery.DATA_LABELS[size] for size in plot_recovery.DATA_SIZES],
            ["4GB", "32GB", "128GB"],
        )
        self.assertIn(
            "Journal/bitmap scan",
            [label for label, _, _ in plot_recovery.PHASES],
        )

    def test_loads_complete_six_point_matrix(self) -> None:
        rows = plot_recovery.load_summary(HERE / "summary.csv")

        self.assertEqual(len(rows), 6)
        self.assertEqual({row.profile for row in rows}, {"ext4-orderless", "AchillesFS"})
        self.assertEqual({row.data_mb for row in rows}, {32, 256, 2048})

    def test_writes_pdf_and_png(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            outputs = plot_recovery.plot_result(
                HERE / "summary.csv", Path(directory)
            )

            self.assertEqual({path.suffix for path in outputs}, {".pdf", ".png"})
            self.assertTrue(all(path.stat().st_size > 0 for path in outputs))


if __name__ == "__main__":
    unittest.main()
