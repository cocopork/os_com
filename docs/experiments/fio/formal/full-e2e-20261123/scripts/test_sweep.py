# SPDX-License-Identifier: MPL-2.0
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sweep.py")
SPEC = importlib.util.spec_from_file_location("full_e2e_sweep", SCRIPT)
sweep = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sweep)


class MatrixTests(unittest.TestCase):
    def run_queue_profile(self, function, profile, device):
        helper = Path(__file__).resolve().parent.parent / "queue-profile.sh"
        command = (
            f"source {helper}; {function} {profile} {device}"
        )
        return subprocess.run(
            ["bash", "-c", command], text=True, capture_output=True, check=False
        )

    def test_queue_profiles_preserve_history_and_align_devices(self):
        expected = {
            ("four", "nvme"): ("4", "60 61 62 63"),
            ("four", "sata"): ("4", "60 61 62 63"),
            ("device-aligned", "nvme"): ("8", "56 57 58 59 60 61 62 63"),
            ("device-aligned", "sata"): ("1", "63"),
        }
        for (profile, device), (queues, cpus) in expected.items():
            queue_result = self.run_queue_profile(
                "virtqueues_for_device", profile, device
            )
            cpu_result = self.run_queue_profile(
                "iothread_cpus_for_device", profile, device
            )
            self.assertEqual(queue_result.returncode, 0, queue_result.stderr)
            self.assertEqual(cpu_result.returncode, 0, cpu_result.stderr)
            self.assertEqual(queue_result.stdout.strip(), queues)
            self.assertEqual(cpu_result.stdout.strip(), cpus)

    def test_queue_profile_rejects_unknown_values(self):
        self.assertNotEqual(
            self.run_queue_profile("virtqueues_for_device", "bad", "nvme").returncode,
            0,
        )
        self.assertNotEqual(
            self.run_queue_profile(
                "virtqueues_for_device", "device-aligned", "bad"
            ).returncode,
            0,
        )

    def test_runner_uses_resolved_queue_count(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn('QUEUE_PROFILE=${QUEUE_PROFILE:-four}', runner)
        self.assertIn('qmp-add-device /benchmark/qmp.sock "$virtqueues"', runner)
        self.assertIn(
            'validation/temporary/rtio-virtio-multiqueue-20261102/scripts',
            runner,
        )

    def test_runner_can_select_device_specific_boot_artifacts(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn("source_for_device()", runner)
        self.assertIn('SOURCE_ART_NVME=${SOURCE_ART_NVME:-$SOURCE_ART}', runner)
        self.assertIn('SOURCE_ART_SATA=${SOURCE_ART_SATA:-$SOURCE_ART}', runner)
        self.assertIn('source=$(source_for_device "$device")', runner)
        self.assertIn('cp --reflink=auto "$source/selected.iso"', runner)

    def test_guest_uses_aligned_128_mib_pagecache_for_both_modes(self):
        guest = Path(__file__).with_name("guest.sh").read_text()
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn('PAGECACHE_CAPACITY_MB=${PAGECACHE_CAPACITY_MB:-128}', runner)
        self.assertIn('PAGECACHE_CAPACITY_MB=%q', runner)
        self.assertIn(
            "NATIVE_OPTIONS=delalloc,rnvpc=off,rnvpc_extend=off,rtio=off,"
            "pagecache_capacity_mb=$PAGECACHE_CAPACITY_MB",
            guest,
        )
        self.assertIn(
            "OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on,"
            "pagecache_capacity_mb=$PAGECACHE_CAPACITY_MB",
            guest,
        )

    def test_guest_supports_runtime_append_and_single_pass_read(self):
        guest = Path(__file__).with_name("guest.sh").read_text()
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn('if [ "$RUNTIME_SECONDS" -gt 0 ]; then', guest)
        self.assertIn('--time_based --runtime="${RUNTIME_SECONDS}s"', guest)
        self.assertIn('RUNTIME_SECONDS=%q', runner)
        self.assertIn('reconfigured-thread-list', runner)

    def test_qemu_uses_host_pid_namespace_for_iothread_affinity(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertRegex(runner, r'docker run -d --name "\$qemu_name" --pid=host')

    def test_device_whitelist_uses_exact_partition_sizes(self):
        runner = Path(__file__).with_name("run-sweep.sh").read_text()
        self.assertIn("nvme) printf '%s\\n' 274877906944", runner)
        self.assertIn("sata) printf '%s\\n' 274877890560", runner)

    def test_full_matrix_has_648_unique_points(self):
        points = sweep.matrix()

        self.assertEqual(len(points), 648)
        self.assertEqual(len({point.case for point in points}), 648)
        self.assertEqual({point.device for point in points}, {"nvme", "sata"})
        self.assertEqual({point.mode for point in points}, {"ext4", "rnvpce"})
        self.assertEqual(
            {point.workload for point in points},
            {"overwrite-randwrite", "append-write", "cold-randread"},
        )
        self.assertEqual({point.repeat for point in points}, {1, 2, 3})

    def test_axis_contracts(self):
        for point in sweep.matrix():
            if point.axis == "threads":
                self.assertEqual(point.bs_kib, 4)
                self.assertIn(point.jobs, sweep.THREADS)
            else:
                self.assertEqual(point.axis, "bs")
                self.assertEqual(point.jobs, 1)
                self.assertIn(point.bs_kib, sweep.BLOCK_KIB)

    def test_format_and_operation_contracts(self):
        for point in sweep.matrix():
            self.assertEqual(point.disk_format, "journal" if point.mode == "ext4" else "nojournal")
            self.assertEqual(point.fio_rw, {
                "overwrite-randwrite": "randwrite",
                "append-write": "write",
                "cold-randread": "randread",
            }[point.workload])
            self.assertEqual(point.prepare, point.workload != "append-write")
            self.assertEqual(point.per_job_mib * point.jobs, point.actual_mib)
            self.assertLessEqual(point.actual_mib, 2048)

    def test_rnvpce_writes_fsync_each_block_but_ext4_and_reads_do_not(self):
        for point in sweep.matrix():
            expected = int(
                point.mode == "rnvpce" and point.workload != "cold-randread"
            )
            self.assertEqual(point.fsync_blocks, expected, point.case)

    def test_runner_passes_fsync_policy_to_guest(self):
        guest = Path(__file__).with_name("guest.sh").read_text()
        runner = Path(__file__).with_name("run-sweep.sh").read_text()

        self.assertIn('--fsync="$FSYNC_BLOCKS"', guest)
        self.assertIn('fsync_blocks=$FSYNC_BLOCKS', guest)
        self.assertIn('FSYNC_BLOCKS=%q', runner)

    def test_pagecache_regression_matrix_has_requested_focused_points(self):
        points = sweep.pagecache_regression_matrix()

        self.assertEqual(len(points), 24)
        self.assertEqual(len({point.case for point in points}), 24)
        self.assertEqual({point.repeat for point in points}, {1, 2, 3})
        coordinates = {
            (point.workload, point.bs_kib, point.jobs)
            for point in points
            if point.repeat == 1
        }
        self.assertEqual(
            coordinates,
            {
                ("append-write", 128, 1),
                ("append-write", 128, 16),
                ("overwrite-randwrite", 4, 1),
                ("overwrite-randwrite", 4, 16),
                ("cold-randread", 4, 1),
                ("cold-randread", 16, 1),
                ("cold-randread", 64, 1),
                ("cold-randread", 256, 1),
            },
        )
        self.assertTrue(all(point.device == "nvme" for point in points))
        self.assertTrue(all(point.mode == "rnvpce" for point in points))
        self.assertTrue(all(point.axis == "pagecache-regression" for point in points))
        self.assertTrue(all(f"{point.bs}-j{point.jobs}" in point.case for point in points))

    def test_reconfigured_thread_matrix_uses_fixed_file_append_and_single_pass_read(self):
        points = sweep.reconfigured_thread_matrix()

        self.assertEqual(len(points), 324)
        self.assertEqual(len({point.case for point in points}), 324)
        self.assertEqual({point.axis for point in points}, {"threads-reconfigured"})
        self.assertEqual(
            {point.workload for point in points},
            {"append-write", "overwrite-randwrite", "cold-randread"},
        )
        append_points = [point for point in points if point.workload == "append-write"]
        overwrite_points = [point for point in points if point.workload == "overwrite-randwrite"]
        read_points = [point for point in points if point.workload == "cold-randread"]
        self.assertTrue(all(point.runtime_seconds == 0 for point in append_points))
        self.assertTrue(all(point.per_job_mib == 1024 for point in append_points))
        self.assertTrue(all(point.actual_mib == 1024 * point.jobs for point in append_points))
        self.assertTrue(all(point.runtime_seconds == 0 for point in overwrite_points))
        self.assertTrue(all(point.per_job_mib == 1024 for point in overwrite_points))
        self.assertTrue(all(point.actual_mib == 1024 * point.jobs for point in overwrite_points))
        self.assertTrue(all(point.prepare for point in overwrite_points))
        self.assertTrue(all(point.runtime_seconds == 0 for point in read_points))
        self.assertTrue(all(point.actual_mib >= 2560 for point in read_points))
        self.assertTrue(all(point.prepare for point in read_points))


class ResultTests(unittest.TestCase):
    def write_fixture(self, root, point, *, operation, bytes_done=None, check_status=0):
        bytes_done = point.expected_bytes if bytes_done is None else bytes_done
        empty = {"io_bytes": 0, "bw_bytes": 0, "iops": 0, "lat_ns": {"mean": 0}, "clat_ns": {"percentile": {}}}
        measured = {
            "io_bytes": bytes_done,
            "bw_bytes": 128 * sweep.MIB,
            "iops": 32768,
            "lat_ns": {"mean": 50000},
            "clat_ns": {"percentile": {"99.000000": 80000}},
        }
        payload = {"jobs": [{"error": 0, "read": measured if operation == "read" else empty,
                              "write": measured if operation == "write" else empty}]}
        (root / "qemu.log").write_text(
            f"FULL_E2E_FIO_START {point.case}\n{json.dumps(payload)}\n"
            f"FULL_E2E_FIO_END {point.case}\n"
            f"FULL_E2E_COMPLETE case={point.case} fio_status=0 sync_status=0 "
            "umount_status=0 start_ns=1000000000 fio_end_ns=3000000000 sync_end_ns=5000000000\n"
        )
        (root / "e2fsck.status").write_text(str(check_status))

    def test_parser_uses_read_section_for_randread(self):
        point = sweep.Point("nvme", "rnvpce", "cold-randread", "bs", 1, 4, 1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, point, operation="read")
            row = sweep.parse_result(root, point)
        self.assertEqual(row["fio_mib_s"], 128)
        self.assertIsNone(row["including_sync_mib_s"])

    def test_parser_rejects_incomplete_io(self):
        point = sweep.Point("sata", "rnvpc", "overwrite-randwrite", "threads", 4, 4, 3)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, point, operation="write", bytes_done=4096)
            with self.assertRaisesRegex(ValueError, "byte count"):
                sweep.parse_result(root, point)

    def test_parser_accepts_dynamic_bytes_for_runtime_append(self):
        point = sweep.Point(
            "nvme", "ext4", "append-write", "threads-reconfigured",
            4, 4, 1, total_mib=0, runtime_seconds=30, file_mib=131072,
        )
        bytes_done = 777 * sweep.MIB
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_fixture(root, point, operation="write", bytes_done=bytes_done)
            row = sweep.parse_result(root, point)
        self.assertEqual(row["actual_mib"], 777)


if __name__ == "__main__":
    unittest.main()
