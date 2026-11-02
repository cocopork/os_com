#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import summarize


class HarnessTests(unittest.TestCase):
    def test_qemu_822_is_rejected_even_with_mapping_help(self):
        with tempfile.TemporaryDirectory() as directory:
            qemu = Path(directory) / "qemu"
            qemu.write_text('#!/bin/sh\ncase "$1" in -version) echo "QEMU emulator version 8.2.2";; -device) echo "iothread-vq-mapping=<list>";; esac\n')
            qemu.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "9.0"):
                summarize.qemu_preflight(str(qemu))

    def test_qemu_mapping_property_is_required(self):
        with tempfile.TemporaryDirectory() as directory:
            qemu = Path(directory) / "qemu"
            qemu.write_text('#!/bin/sh\ncase "$1" in -version) echo "QEMU emulator version 10.2.1";; -device) echo "num-queues=<uint16>";; esac\n')
            qemu.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "iothread-vq-mapping"):
                summarize.qemu_preflight(str(qemu))

    def test_selects_four_distinct_hctx_and_cpus(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for hctx, cpus in [("0", "2-3"), ("1", "3-4"), ("3", "5"), ("7", "6-7")]:
                path = root / hctx
                path.mkdir()
                (path / "cpu_list").write_text(cpus + "\n")
            selected = summarize.select_hctx_cpus(root, 4, {2, 3, 4, 5, 6, 7})
            self.assertEqual([(row["hctx"], row["cpu"]) for row in selected], [(0, 2), (1, 3), (3, 5), (7, 6)])
            self.assertTrue(all(row["source"].endswith(f'/{row["hctx"]}/cpu_list') for row in selected))

    def test_target_rejects_mounted_and_wrong_size(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lsblk = root / "lsblk"
            findmnt = root / "findmnt"
            lsblk.write_text('#!/bin/sh\necho "${LSBLK_FIXTURE}"\n')
            findmnt.write_text('#!/bin/sh\nexit "${FINDMNT_STATUS}"\n')
            lsblk.chmod(0o755)
            findmnt.chmod(0o755)
            target = root / "nvme1n1p1"
            target.touch()
            fixture = json.dumps({"blockdevices": [{"name": "nvme1n1p1", "type": "part", "size": 274877906944, "pkname": "nvme1n1"}]})
            with mock.patch("summarize.stat.S_ISBLK", return_value=True):
                with mock.patch.dict(os.environ, {"LSBLK_FIXTURE": fixture, "FINDMNT_STATUS": "0"}):
                    with self.assertRaisesRegex(ValueError, "mounted"):
                        summarize.target_preflight(str(target), str(lsblk), str(findmnt))
                wrong = json.dumps({"blockdevices": [{"name": "nvme1n1p1", "type": "part", "size": 123, "pkname": "nvme1n1"}]})
                with mock.patch.dict(os.environ, {"LSBLK_FIXTURE": wrong, "FINDMNT_STATUS": "1"}):
                    with self.assertRaisesRegex(ValueError, "256 GiB"):
                        summarize.target_preflight(str(target), str(lsblk), str(findmnt))

    def test_mapping_is_explicit_one_to_one(self):
        self.assertEqual(summarize.qemu_mapping(4), [
            {"iothread": "iothread0", "vqs": [0]},
            {"iothread": "iothread1", "vqs": [1]},
            {"iothread": "iothread2", "vqs": [2]},
            {"iothread": "iothread3", "vqs": [3]},
        ])

    def test_qmp_device_add_carries_structured_mapping(self):
        args = summarize.qmp_device_args(4)
        self.assertEqual(args["driver"], "virtio-blk-pci")
        self.assertEqual(args["bus"], "target-port")
        self.assertEqual(args["disable-legacy"], "on")
        self.assertEqual(args["num-queues"], 4)
        self.assertEqual(args["iothread-vq-mapping"], [
            {"iothread": "iothread0", "vqs": [0]},
            {"iothread": "iothread1", "vqs": [1]},
            {"iothread": "iothread2", "vqs": [2]},
            {"iothread": "iothread3", "vqs": [3]},
        ])

    def test_acceptance_requires_four_hctx_and_no_more_than_70_percent(self):
        self.assertFalse(summarize.summarize_counts({0: 10, 1: 10, 2: 10})["accepted"])
        self.assertFalse(summarize.summarize_counts({0: 71, 1: 10, 2: 10, 3: 9})["accepted"])
        result = summarize.summarize_counts({0: 40, 1: 30, 2: 20, 3: 10})
        self.assertTrue(result["accepted"])
        self.assertEqual(result["counts"], {"0": 40, "1": 30, "2": 20, "3": 10})

    def test_trace_parser_preserves_raw_hctx_ids(self):
        result = summarize.parse_trace("@hctx[2]: 40\n@hctx[7]: 30\n@hctx[11]: 20\n@hctx[13]: 10\n")
        self.assertEqual(result["counts"], {"2": 40, "7": 30, "11": 20, "13": 10})
        self.assertTrue(result["accepted"])


if __name__ == "__main__":
    unittest.main()
