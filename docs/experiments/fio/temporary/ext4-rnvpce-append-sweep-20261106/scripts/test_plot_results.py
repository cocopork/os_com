#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
from pathlib import Path
import tempfile
import unittest

import plot_results


class AppendPlotTests(unittest.TestCase):
    def test_legend_describes_filesystems_and_optional_repeat_samples(self):
        self.assertEqual(
            [handle.get_label() for handle in plot_results._legend_handles(False)],
            ["EXT4 writeback", "rNVPCe (4 workers)"],
        )
        self.assertEqual(
            plot_results._legend_handles(True)[-1].get_label(),
            "repeat samples (fio)",
        )

    def test_repeat_samples_expand_y_axis_instead_of_being_clipped(self):
        with tempfile.TemporaryDirectory() as directory:
            csv_path = Path(directory) / "repeats.csv"
            csv_path.write_text(
                "case,mode,axis,workload,value,bs_kib,jobs,actual_mib,fio_mib_s,"
                "including_sync_mib_s,iops,mean_latency_us,p99_latency_us,round\n"
                "append-ext4-threads-1,ext4,threads,append,1,4,1,2048,200,180,1,1,1,round-1\n"
            )
            figure, axis = plot_results.plt.subplots()
            axis.set_ylim(0, 100)

            plot_results._overlay_repeats(axis, csv_path, "threads")

            self.assertGreater(axis.get_ylim()[1], 200)
            plot_results.plt.close(figure)

    def test_render_creates_both_sweep_images(self):
        source = Path(
            "/home/zn/asterinas-build-env/"
            "ext4-rnvpce-append-sweep-20261106/results/results.csv"
        )
        if not source.exists():
            self.skipTest("validated experiment CSV is unavailable")
        with tempfile.TemporaryDirectory() as directory:
            outputs = plot_results.render(source, Path(directory))
            self.assertEqual(
                {path.name for path in outputs},
                {
                    "ext4-rnvpce-4worker-append-thread-sweep.png",
                    "ext4-rnvpce-4worker-append-bs-sweep.png",
                },
            )
            self.assertTrue(all(path.stat().st_size > 40_000 for path in outputs))


if __name__ == "__main__":
    unittest.main()
