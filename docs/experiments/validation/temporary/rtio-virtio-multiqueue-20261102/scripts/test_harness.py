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
    def test_fio_json_is_emitted_only_after_sync_and_unmount(self):
        guest = (Path(__file__).parent / "guest.sh").read_text()
        self.assertIn('--output="$FIO_JSON"', guest)
        self.assertGreater(guest.index('cat "$FIO_JSON"'), guest.rindex('umount "$MNT"'))

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
            fuser = root / "fuser"
            lsblk.write_text('#!/bin/sh\nfor last; do :; done\ncase "$last" in /dev/nvme1n1p1) printf "%s\\n" "$LSBLK_TARGET";; *) printf "%s\\n" "$LSBLK_PARENT";; esac\n')
            findmnt.write_text('#!/bin/sh\nprintf "%s\\n" "$FINDMNT_FIXTURE"\n')
            fuser.write_text('#!/bin/sh\nexit "${FUSER_STATUS:-1}"\n')
            for command in (lsblk, findmnt, fuser):
                command.chmod(0o755)
            target_data = {"blockdevices": [{"name": "nvme1n1p1", "type": "part", "size": 274877906944,
                                             "pkname": "nvme1n1", "maj:min": "259:1", "mountpoints": [None]}]}
            parent_data = {"blockdevices": [{"name": "nvme1n1", "type": "disk", "maj:min": "259:0", "mountpoints": [None]}]}
            swaps = root / "swaps"
            swaps.write_text("Filename Type Size Used Priority\n")
            for device_id in ("259:0", "259:1"):
                (root / "dev" / "block" / device_id / "holders").mkdir(parents=True)
            kwargs = dict(sys_dev_root=root / "dev" / "block", swaps_path=swaps, fuser_bin=str(fuser),
                          identity_fn=lambda path: "259:1" if path.endswith("p1") else "259:0",
                          canonical_fn=lambda path: path)
            mounted = {"filesystems": [{"source": "/dev/disk/by-id/alias", "maj:min": "259:1", "target": "/mnt/test"}]}
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps(mounted)}):
                with self.assertRaisesRegex(ValueError, "mounted"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt), **kwargs)
            target_data["blockdevices"][0]["size"] = 123
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps({"filesystems": []})}):
                with self.assertRaisesRegex(ValueError, "256 GiB"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt), **kwargs)
            target_data["blockdevices"][0]["size"] = 274877906944
            swaps.write_text("Filename Type Size Used Priority\n/dev/nvme1n1p1 partition 1024 0 -2\n")
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps({"filesystems": []})}):
                with self.assertRaisesRegex(ValueError, "swap"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt), **kwargs)
            swaps.write_text("Filename Type Size Used Priority\n")
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps({"filesystems": []}), "FUSER_STATUS": "0"}):
                with self.assertRaisesRegex(ValueError, "in use"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt), **kwargs)

            fuser.write_text('#!/bin/sh\necho "lookup failed" >&2\nexit 1\n')
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps({"filesystems": []})}):
                with self.assertRaisesRegex(ValueError, "fuser"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt), **kwargs)

    def test_target_rejects_alternate_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            lsblk = Path(directory) / "lsblk"
            lsblk.write_text('#!/bin/sh\necho "${LSBLK_FIXTURE}"\n')
            lsblk.chmod(0o755)
            fixture = json.dumps({"blockdevices": [{"name": "nvme1n1p1", "type": "part", "size": 274877906944, "pkname": "nvme0n1", "maj:min": "259:1"}]})
            with mock.patch.dict(os.environ, {"LSBLK_FIXTURE": fixture}):
                with self.assertRaisesRegex(ValueError, "nvme1n1"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), identity_fn=lambda _: "259:1", canonical_fn=lambda path: path)

    def test_target_rejects_mounted_dm_descendant_by_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lsblk = root / "lsblk"
            findmnt = root / "findmnt"
            fuser = root / "fuser"
            lsblk.write_text('#!/bin/sh\nfor last; do :; done\ncase "$last" in /dev/nvme1n1p1) printf "%s\\n" "$LSBLK_TARGET";; *) printf "%s\\n" "$LSBLK_PARENT";; esac\n')
            findmnt.write_text('#!/bin/sh\nprintf "%s\\n" "$FINDMNT_FIXTURE"\n')
            fuser.write_text('#!/bin/sh\nexit 1\n')
            for command in (lsblk, findmnt, fuser):
                command.chmod(0o755)
            target_data = {"blockdevices": [{"name": "nvme1n1p1", "type": "part", "size": 274877906944,
                                             "pkname": "nvme1n1", "maj:min": "259:1", "mountpoints": [None],
                                             "children": [{"name": "dm-0", "type": "crypt", "maj:min": "253:0", "mountpoints": ["/secret"]}]}]}
            parent_data = {"blockdevices": [{"name": "nvme1n1", "type": "disk", "maj:min": "259:0", "mountpoints": [None]}]}
            mounts = {"filesystems": [{"source": "/dev/mapper/secret", "maj:min": "253:0", "target": "/secret"}]}
            swaps = root / "swaps"
            swaps.write_text("Filename Type Size Used Priority\n")
            holders = root / "dev" / "block" / "259:1" / "holders"
            holders.mkdir(parents=True)
            (holders / "dm-0").touch()
            (root / "dev" / "block" / "259:0" / "holders").mkdir(parents=True)
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps(mounts)}):
                with self.assertRaisesRegex(ValueError, "descendant"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt),
                                               sys_dev_root=root / "dev" / "block", swaps_path=swaps,
                                               fuser_bin=str(fuser), identity_fn=lambda path: "259:1" if path.endswith("p1") else "259:0",
                                               canonical_fn=lambda path: path)
            target_data["blockdevices"][0].pop("children")
            with mock.patch.dict(os.environ, {"LSBLK_TARGET": json.dumps(target_data), "LSBLK_PARENT": json.dumps(parent_data), "FINDMNT_FIXTURE": json.dumps(mounts)}):
                with self.assertRaisesRegex(ValueError, "holder"):
                    summarize.target_preflight("/dev/nvme1n1p1", str(lsblk), str(findmnt),
                                               sys_dev_root=root / "dev" / "block", swaps_path=swaps,
                                               fuser_bin=str(fuser), identity_fn=lambda path: "259:1" if path.endswith("p1") else "259:0",
                                               canonical_fn=lambda path: path)

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
        result = summarize.parse_trace("READY parent=nvme1n1\nTRACE_FINAL_BEGIN\n@hctx[2]: 40\n@hctx[7]: 30\n@hctx[11]: 20\n@hctx[13]: 10\nTRACE_FINAL_END\n")
        self.assertEqual(result["counts"], {"2": 40, "7": 30, "11": 20, "13": 10})
        self.assertTrue(result["accepted"])

    def test_trace_parser_rejects_balanced_partial_map(self):
        raw = "Attaching 1 probe...\nREADY parent=nvme1n1\n@hctx[0]: 10\n@hctx[1]: 10\n@hctx[2]: 10\n@hctx[3]: 10\n"
        with self.assertRaisesRegex(ValueError, "final"):
            summarize.parse_trace(raw)


if __name__ == "__main__":
    unittest.main()
