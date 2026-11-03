# SPDX-License-Identifier: MPL-2.0
import json
import unittest
import results

class ResultTests(unittest.TestCase):
    def sample(self, *, error=0, size=2147483648, sync=0):
        fio = {'jobs': [{'error': error, 'write': {'io_bytes': size, 'bw_bytes': 104857600,
                 'lat_ns': {'mean': 1000}, 'clat_ns': {'percentile': {'99.000000': 2000}}}}]}
        return ('PRELIM_FIO_START control\n' + json.dumps(fio) + '\nPRELIM_FIO_END control\n'
                f'PRELIM_COMPLETE control fio_status=0 start_ns=1000000000 fio_end_ns=21000000000 sync_end_ns=22000000000 sync_status={sync} umount_status=0\n')

    def test_rejects_short_write_fio_error_and_failed_sync(self):
        for options in ({'size': 4096}, {'error': 5}, {'sync': 1}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                results.parse_guest(self.sample(**options), 'control')

    def test_mechanism_rejects_idle_worker_wrong_queue_and_extra_flush(self):
        profiles = [{'name': 'rtio_worker', 'device': 'vdc', 'worker': str(i),
                     'queue': str(i), 'submitted_bios': '1',
                     'data_complete_depth': '0', 'durable_result_depth': '0',
                     'queued_dirty_pages': '0', 'in_flight_pages': '0',
                     'completion_pending_pages': '0', 'owned_inodes': '0'}
                    for i in range(4)]
        profiles += [{'name': 'block_submission_queue', 'device': 'vdc', 'queue': str(i),
                      'in_flight_requests': '0', 'staging_depth': '0'} for i in range(4)]
        profiles += [{'name': 'rtio_multi_worker', 'configured': '4', 'active': '0',
                      'epochs': '2', 'device_flushes': '2'}]
        self.assertTrue(all(results.mechanism_gates(profiles, 4, True).values()))
        for row, key, value in ((0, 'submitted_bios', '0'), (1, 'queue', '0'),
                                (2, 'durable_result_depth', '1'),
                                (8, 'device_flushes', '3'), (4, 'in_flight_requests', '1')):
            original = profiles[row][key]
            profiles[row][key] = value
            self.assertFalse(all(results.mechanism_gates(profiles, 4, True).values()))
            profiles[row][key] = original
        self.assertFalse(all(results.mechanism_gates(profiles, 4, False).values()))
        self.assertFalse(all(results.mechanism_gates([], 4, True).values()))

    def test_cpu_trace_interrupting_fio_json_is_removed(self):
        raw = self.sample().replace('"write"', '"w[CPU-ISOLATION] role=User cpu=7\r\nrite"')
        self.assertEqual(results.parse_guest(raw, 'control')['fio_mib_s'], 100)

    def test_bare_serial_cursor_prefix_before_json_property_is_removed(self):
        raw = self.sample().replace('"write"', '\x1b[      "write"')
        self.assertEqual(results.parse_guest(raw, 'control')['fio_mib_s'], 100)

    def test_bare_serial_cursor_prefix_before_json_number_is_removed(self):
        raw = self.sample().replace('104857600', '\x1b[ 104857600')
        self.assertEqual(results.parse_guest(raw, 'control')['fio_mib_s'], 100)

    def test_rates_include_sync_and_preserve_latency(self):
        result = results.parse_guest(self.sample(), 'control')
        self.assertEqual(result['fio_mib_s'], 100)
        self.assertAlmostEqual(result['including_sync_mib_s'], 2048/21)
        self.assertEqual(result['mean_latency_ns'], 1000)
        self.assertEqual(result['p99_latency_ns'], 2000)

if __name__ == '__main__':
    unittest.main()
