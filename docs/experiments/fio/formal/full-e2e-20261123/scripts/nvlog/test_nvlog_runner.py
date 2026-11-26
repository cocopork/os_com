import json
import pathlib
import tempfile
import unittest

import nvlog_runner


class NvlogRunnerTests(unittest.TestCase):
    def test_nvpc_config_converts_mib_to_pages(self):
        config = nvlog_runner.nvpc_config(nvm_mib=64)
        self.assertIn("nvpc_sz=16384", config)
        self.assertIn("absorb_syn=1", config)
        self.assertIn("rebuild=0", config)

    def test_device_contract_rejects_wrong_size_or_mounted_device(self):
        with self.assertRaisesRegex(ValueError, "unexpected size"):
            nvlog_runner.validate_device_contract(
                canonical_path="/dev/nvme1n1p2",
                actual_size=1,
                mounted=False,
                swap=False,
                holders=(),
                open_users=(),
            )
        with self.assertRaisesRegex(ValueError, "mounted"):
            nvlog_runner.validate_device_contract(
                canonical_path="/dev/sda4",
                actual_size=274877890560,
                mounted=True,
                swap=False,
                holders=(),
                open_users=(),
            )

    def test_cache_contract_adds_anon_baseline_and_file_budget(self):
        contract = nvlog_runner.cache_contract(
            anon_baseline_bytes=180 * 1024 * 1024,
            file_budget_mib=64,
            headroom_mib=32,
            hard_limit_mib=512,
        )
        self.assertEqual(contract["memory_high_bytes"], 276 * 1024 * 1024)
        self.assertEqual(contract["memory_max_bytes"], 512 * 1024 * 1024)
        self.assertEqual(contract["target_file_bytes"], 64 * 1024 * 1024)
        self.assertEqual(contract["allowed_file_deviation_percent"], 10)

    def test_complete_result_requires_all_success_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            point = pathlib.Path(tmp)
            (point / "result.json").write_text(
                json.dumps(
                    {
                        "benchmark_exit": 0,
                        "sync_exit": 0,
                        "unmount_exit": 0,
                        "qemu_exit": 0,
                        "e2fsck_exit": 0,
                        "parse_valid": True,
                    }
                )
            )
            self.assertTrue(nvlog_runner.result_is_complete(point))
            payload = json.loads((point / "result.json").read_text())
            payload["e2fsck_exit"] = 4
            (point / "result.json").write_text(json.dumps(payload))
            self.assertFalse(nvlog_runner.result_is_complete(point))


if __name__ == "__main__":
    unittest.main()
