#!/usr/bin/env python3

import unittest

import matrix
from qmp_helper import iothread_vq_mapping, qmp_device_arguments


class VirtioContentionProfileMatrixTest(unittest.TestCase):
    def test_matrix_has_one_mode_and_three_thread_points(self):
        points = matrix.matrix()
        self.assertEqual(len(points), 3)
        self.assertEqual({point.mode for point in points}, {"8q-module-profile"})
        self.assertEqual({point.jobs for point in points}, {4, 16, 32})
        self.assertTrue(all(point.bs_kib == 4 for point in points))
        self.assertTrue(all(point.repeats == 3 for point in points))

    def test_modes_keep_eight_queues_and_host_iothreads(self):
        for point in matrix.matrix():
            self.assertEqual(point.iothreads, 8)
            self.assertEqual(point.virtqueues, 8)

    def test_eight_queues_map_one_per_iothread(self):
        mapping = iothread_vq_mapping(8, 8)
        self.assertEqual(mapping[0], {"iothread": "iothread0", "vqs": [0]})
        self.assertEqual(mapping[7], {"iothread": "iothread7", "vqs": [7]})
        self.assertEqual(
            sorted(queue for item in mapping for queue in item["vqs"]),
            list(range(8)),
        )

    def test_qmp_properties_use_qemu_schema_types(self):
        arguments = qmp_device_arguments(8, 8)
        self.assertEqual(arguments["disable-legacy"], "on")
        for name in ("request-merging", "discard", "write-zeroes", "event_idx",
                     "indirect_desc"):
            self.assertIs(arguments[name], False)


if __name__ == "__main__":
    unittest.main()
