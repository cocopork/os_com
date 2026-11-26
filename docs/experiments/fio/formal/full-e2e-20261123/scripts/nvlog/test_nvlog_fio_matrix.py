import json
import pathlib
import tempfile
import unittest

import nvlog_fio_matrix


class NvlogFioMatrixTests(unittest.TestCase):
    def test_write_points_fsync_each_block_but_reads_do_not(self):
        for point in nvlog_fio_matrix.matrix():
            expected = 0 if point.workload == "cold-randread" else 1
            self.assertEqual(point.fsync_blocks, expected, point.case)

    def test_runner_uses_device_queue_profile(self):
        runner = pathlib.Path(__file__).with_name("run-fio.sh").read_text()
        self.assertIn('QUEUE_PROFILE=${QUEUE_PROFILE:-four}', runner)
        self.assertIn('virtqueues=$(virtqueues_for_device "$QUEUE_PROFILE" "$device")', runner)
        self.assertIn('-device "virtio-blk-pci,drive=target,num-queues=$virtqueues"', runner)

    def test_guest_supports_runtime_append_and_single_pass_read(self):
        guest = pathlib.Path(__file__).with_name("guest-fio.sh").read_text()
        runner = pathlib.Path(__file__).with_name("run-fio.sh").read_text()
        self.assertIn('runtime_seconds=$(cmdline_value fio_runtime_seconds)', guest)
        self.assertIn('--time_based --runtime="${runtime_seconds}s"', guest)
        self.assertIn('timeout --preserve-status --signal=INT --kill-after=30', guest)
        self.assertIn('reconfigured-thread-list', runner)
        self.assertIn('fio_runtime_seconds=$runtime_seconds', runner)

    def test_guest_samples_nvlog_usage_while_fio_is_running(self):
        guest = pathlib.Path(__file__).with_name("guest-fio.sh").read_text()
        monitor = guest.split('while kill -0 "$fio_pid"', 1)[1].split("done", 1)[0]
        self.assertIn("nvpcctl usage", monitor)

    def test_matrix_matches_rnvpce_coordinates(self):
        points = nvlog_fio_matrix.matrix()
        self.assertEqual(len(points), 324)
        self.assertEqual({p.device for p in points}, {"nvme", "sata"})
        self.assertEqual({p.workload for p in points}, {
            "overwrite-randwrite", "append-write", "cold-randread"
        })
        self.assertEqual(
            {p.jobs for p in points if p.axis == "threads"},
            {1, 4, 8, 12, 16, 20, 24, 28, 32},
        )
        self.assertEqual(
            {p.bs_kib for p in points if p.axis == "bs"},
            {4, 8, 16, 32, 64, 128, 256, 512, 1024},
        )

    def test_point_preserves_two_gib_contract(self):
        point = nvlog_fio_matrix.point_by_case(
            "e2e-nvme-nvlog-overwrite-randwrite-threads-32-r1"
        )
        self.assertEqual(point.per_job_mib, 64)
        self.assertEqual(point.actual_mib, 2048)
        self.assertEqual(point.expected_bytes, 2048 * 1024 * 1024)
        self.assertTrue(point.prepare)

    def test_reconfigured_thread_matrix_matches_fixed_file_append_contract(self):
        points = nvlog_fio_matrix.reconfigured_thread_matrix()

        self.assertEqual(len(points), 162)
        self.assertEqual(len({point.case for point in points}), 162)
        self.assertEqual({point.axis for point in points}, {"threads-reconfigured"})
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

    def test_parser_requires_clean_lifecycle_and_exact_bytes(self):
        point = nvlog_fio_matrix.point_by_case(
            "e2e-sata-nvlog-append-write-bs-128k-r1"
        )
        fio = {
            "jobs": [{
                "error": 0,
                "write": {
                    "io_bytes": point.expected_bytes,
                    "bw_bytes": 1048576,
                    "iops": 8.0,
                    "lat_ns": {"mean": 1000},
                    "clat_ns": {"percentile": {"99.000000": 2000}},
                },
            }]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            path.joinpath("qemu.log").write_text(
                "FULL_E2E_FIO_START " + point.case + "\n"
                + json.dumps(fio) + "\nFULL_E2E_FIO_END " + point.case + "\n"
                + "FULL_E2E_CACHE_SAMPLE time=1 anon=4096 file=134217728 file_dirty=0 file_writeback=0\n"
                + "FULL_E2E_MEMORY_EVENT_HIGH=4\n"
                + "FULL_E2E_MEMORY_EVENT_OOM_KILL=0\n"
                + "nvpcctl: used\t42 pages\n"
                + "nvpcctl: used\t0 pages\n"
                + "FULL_E2E_COMPLETE case=" + point.case
                + " fio_status=0 sync_status=0 close_status=0 umount_status=0"
                + " start_ns=100 fio_end_ns=200 sync_end_ns=300\n"
            )
            path.joinpath("e2fsck.status").write_text("0\n")
            result = nvlog_fio_matrix.parse_result(path, point)
            self.assertEqual(result["fio_mib_s"], 1.0)
            self.assertEqual(result["including_sync_mib_s"], 2048 / 0.0000002)
            self.assertEqual(result["cache_file_peak_bytes"], 134217728)
            self.assertEqual(result["memory_events"]["high"], 4)
            self.assertEqual(result["nvlog_used_pages"], 42)

    def test_parser_rejects_write_that_did_not_use_nvlog(self):
        point = nvlog_fio_matrix.point_by_case(
            "e2e-sata-nvlog-append-write-bs-128k-r1"
        )
        fio = {
            "jobs": [{
                "error": 0,
                "write": {
                    "io_bytes": point.expected_bytes,
                    "bw_bytes": 1048576,
                    "iops": 8.0,
                    "lat_ns": {"mean": 1000},
                    "clat_ns": {"percentile": {"99.000000": 2000}},
                },
            }]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            path.joinpath("qemu.log").write_text(
                "FULL_E2E_FIO_START " + point.case + "\n"
                + json.dumps(fio) + "\nFULL_E2E_FIO_END " + point.case + "\n"
                + "FULL_E2E_MEMORY_EVENT_OOM_KILL=0\n"
                + "nvpcctl: used\t0 pages\n"
                + "FULL_E2E_COMPLETE case=" + point.case
                + " fio_status=0 sync_status=0 close_status=0 umount_status=0"
                + " start_ns=100 fio_end_ns=200 sync_end_ns=300\n"
            )
            path.joinpath("e2fsck.status").write_text("0\n")
            with self.assertRaisesRegex(ValueError, "did not use NVLog"):
                nvlog_fio_matrix.parse_result(path, point)

    def test_parser_accepts_dynamic_bytes_for_runtime_append(self):
        point = nvlog_fio_matrix.Point(
            "nvme", "append-write", "threads-reconfigured", 4, 4, 1,
            total_mib=0, runtime_seconds=30, file_mib=131072,
        )
        bytes_done = 777 * nvlog_fio_matrix.MIB
        fio = {
            "jobs": [{
                "error": 0,
                "write": {
                    "io_bytes": bytes_done,
                    "bw_bytes": 1048576,
                    "iops": 8.0,
                    "lat_ns": {"mean": 1000},
                    "clat_ns": {"percentile": {"99.000000": 2000}},
                },
            }]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            path.joinpath("qemu.log").write_text(
                "FULL_E2E_FIO_START " + point.case + "\n"
                + json.dumps(fio) + "\nFULL_E2E_FIO_END " + point.case + "\n"
                + "FULL_E2E_MEMORY_EVENT_OOM_KILL=0\n"
                + "nvpcctl: used\t42 pages\n"
                + "FULL_E2E_COMPLETE case=" + point.case
                + " fio_status=0 sync_status=0 close_status=0 umount_status=0"
                + " start_ns=100 fio_end_ns=200 sync_end_ns=300\n"
            )
            path.joinpath("e2fsck.status").write_text("0\n")
            result = nvlog_fio_matrix.parse_result(path, point)
        self.assertEqual(result["actual_mib"], 777)


if __name__ == "__main__":
    unittest.main()
