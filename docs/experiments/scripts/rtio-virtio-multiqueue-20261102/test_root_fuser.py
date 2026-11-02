#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import contextlib
import io
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import root_fuser_check


class RootFuserTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.uid_map = self.root / "uid_map"
        self.uid_map.write_text("0 0 4294967295\n")
        self.stdout = self.root / "fuser.stdout"
        self.stderr = self.root / "fuser.stderr"
        self.calls = []

    def stat_device(self, path):
        self.calls.append(path)
        devices = {"/dev/nvme1n1p1": (259, 1), "/dev/nvme1n1": (259, 0)}
        major, minor = devices[path]
        return SimpleNamespace(st_mode=stat.S_IFBLK, st_rdev=os.makedev(major, minor))

    def call_check(self, status=1, stderr=b"", stat_fn=None):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = root_fuser_check.check(
                "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0",
                self.stdout, self.stderr, uid_map_path=self.uid_map,
                euid_fn=lambda: 0, stat_fn=stat_fn or self.stat_device,
                realpath_fn=lambda path: path,
                run_fn=lambda args, **kwargs: subprocess.CompletedProcess(args, status, b"", stderr),
            )
        return result, output.getvalue()

    def test_rejects_bad_uid_mapping_or_zero_span_before_device_lookup(self):
        for text in ("0 1000 1\n", "0 0 0\n", "1 0 1\n"):
            self.uid_map.write_text(text)
            with self.assertRaisesRegex(ValueError, "uid_map"):
                self.call_check()
            self.assertEqual(self.calls, [])

    def test_rejects_missing_nonblock_or_mismatched_device_before_fuser(self):
        for stat_fn in (
            lambda path: (_ for _ in ()).throw(FileNotFoundError(path)),
            lambda path: SimpleNamespace(st_mode=stat.S_IFREG, st_rdev=0),
            lambda path: SimpleNamespace(st_mode=stat.S_IFBLK, st_rdev=os.makedev(259, 9)),
        ):
            with self.assertRaisesRegex(ValueError, "block|major:minor"):
                self.call_check(stat_fn=stat_fn)
            self.assertFalse(self.stdout.exists())

    def test_status_one_requires_empty_stderr_and_writes_separate_files(self):
        status, marker = self.call_check()
        self.assertEqual(status, 1)
        self.assertEqual(marker.strip(), "ROOT_FUSER_IDENTITY_VALIDATED")
        self.assertEqual(self.stdout.read_bytes(), b"")
        self.assertEqual(self.stderr.read_bytes(), b"")
        with self.assertRaisesRegex(ValueError, "stderr"):
            self.call_check(stderr=b"lookup failed")
        self.assertEqual(self.stderr.read_bytes(), b"lookup failed")


if __name__ == "__main__":
    unittest.main()
