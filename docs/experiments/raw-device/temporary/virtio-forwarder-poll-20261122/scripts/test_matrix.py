#!/usr/bin/env python3

import unittest

import matrix
from qmp_helper import iothread_vq_mapping, qmp_device_arguments


class VirtioForwarderPollMatrixTest(unittest.TestCase):
    def test_matrix_has_three_modes_and_five_thread_points(self):
        points = matrix.matrix()
        self.assertEqual(len(points), 15)
        self.assertEqual({point.mode for point in points}, {
            "8q-8w-wait", "32q-8w-wait", "32q-8w-poll",
        })
        self.assertEqual({point.jobs for point in points}, {8, 16, 24, 28, 32})
        self.assertTrue(all(point.bs_kib == 4 for point in points))
        self.assertTrue(all(point.repeats == 3 for point in points))

    def test_modes_keep_eight_host_iothreads(self):
        for point in matrix.matrix():
            self.assertEqual(point.iothreads, 8)
        self.assertEqual(matrix.point("8q-8w-wait", 8).virtqueues, 8)
        self.assertEqual(matrix.point("32q-8w-wait", 8).virtqueues, 32)
        self.assertEqual(matrix.point("32q-8w-poll", 8).virtqueues, 32)

    def test_thirty_two_queues_map_four_per_iothread(self):
        mapping = iothread_vq_mapping(32, 8)
        self.assertEqual(mapping[0], {"iothread": "iothread0", "vqs": [0, 8, 16, 24]})
        self.assertEqual(mapping[7], {"iothread": "iothread7", "vqs": [7, 15, 23, 31]})
        self.assertEqual(
            sorted(queue for item in mapping for queue in item["vqs"]),
            list(range(32)),
        )

    def test_qmp_properties_use_qemu_schema_types(self):
        arguments = qmp_device_arguments(32, 8)
        self.assertEqual(arguments["disable-legacy"], "on")
        for name in ("request-merging", "discard", "write-zeroes", "event_idx",
                     "indirect_desc"):
            self.assertIs(arguments[name], False)


if __name__ == "__main__":
    unittest.main()
