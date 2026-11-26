import unittest

import mixed_matrix


class MixedMatrixTests(unittest.TestCase):
    def test_matrix_has_144_nvlog_points(self):
        points = mixed_matrix.matrix()
        self.assertEqual(len(points), 144)
        self.assertEqual(len({point.case for point in points}), 144)
        self.assertTrue(all(point.mode == "nvlog" for point in points))

    def test_device_queue_contract(self):
        points = mixed_matrix.matrix()
        self.assertEqual({p.virtqueues for p in points if p.device == "nvme"}, {8})
        self.assertEqual({p.virtqueues for p in points if p.device == "sata"}, {1})

    def test_workload_and_cache_contract(self):
        point = mixed_matrix.point_by_case("mixed-nvme-nvlog-r30w70-sync40-r2")
        self.assertEqual(point.threads, 8)
        self.assertEqual(point.file_mib, 320)
        self.assertEqual(point.dataset_mib, 2560)
        self.assertEqual(point.seed, 20261119)
        self.assertEqual(point.dram_cache_mib, 128)
        self.assertEqual(point.nv_data_cache_mib, 128)


if __name__ == "__main__":
    unittest.main()
