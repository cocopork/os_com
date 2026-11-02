#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import runpy
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import root_fuser_check


def helper_fixture(argv):
    """Execute the real helper entrypoint behind a read-only Docker/chroot fixture."""
    prefix = ["run", "--rm", "--privileged", "--pid=host", "--userns=host",
              "--network", "none", "-v", "/:/host:ro", "fixture-tools",
              "chroot", "/host", "/usr/bin/python3"]
    if argv[:len(prefix)] != prefix:
        raise AssertionError(f"unexpected helper isolation: {argv}")
    helper, *args = argv[len(prefix):]
    if args != ["check", "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0"]:
        raise AssertionError(f"unexpected helper arguments: {args}")

    original_open = io.open
    original_os_open = os.open
    original_stat = os.stat

    def readonly_open(path, mode="r", *args, **kwargs):
        if any(flag in mode for flag in "wax+"):
            raise OSError("fixture chroot is read-only")
        if str(path) == "/proc/self/uid_map":
            return io.StringIO("0 0 4294967295\n")
        return original_open(path, mode, *args, **kwargs)

    def readonly_os_open(path, flags, *args, **kwargs):
        if flags & (os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND):
            raise OSError("fixture chroot is read-only")
        return original_os_open(path, flags, *args, **kwargs)

    def stat_device(path, *args, **kwargs):
        if path in ("/dev/nvme1n1p1", "/dev/nvme1n1"):
            return SimpleNamespace(st_mode=stat.S_IFBLK,
                                   st_rdev=os.makedev(259, int(path.endswith("p1"))))
        return original_stat(path, *args, **kwargs)

    def run_fuser(command, **kwargs):
        if command != ["/usr/bin/fuser", "-s", "/dev/nvme1n1p1", "/dev/nvme1n1"]:
            raise AssertionError(f"unexpected fuser command: {command}")
        return subprocess.CompletedProcess(command, int(os.environ.get("FIXTURE_FUSER_STATUS", "1")),
                                           b"", os.environ.get("FIXTURE_FUSER_STDERR", "").encode())

    output = io.StringIO()
    with mock.patch("builtins.open", readonly_open), mock.patch("io.open", readonly_open), \
            mock.patch("os.open", readonly_os_open), mock.patch("os.geteuid", return_value=0), \
            mock.patch("os.stat", stat_device), mock.patch("os.path.realpath", side_effect=lambda path: path), \
            mock.patch("subprocess.run", run_fuser), mock.patch.object(sys, "argv", [helper, *args]), \
            contextlib.redirect_stdout(output):
        try:
            runpy.run_path(helper, run_name="__main__")
        except SystemExit as error:
            status = error.code
    result = output.getvalue()
    mode = os.environ.get("FIXTURE_RESULT_MODE", "valid")
    if mode == "malformed":
        result = "not JSON"
    elif mode != "valid":
        record = json.loads(result)
        if mode == "missing_identity":
            del record["identity"]
        elif mode == "wrong_target":
            record["target"]["major_minor"] = "259:9"
        elif mode == "wrong_parent":
            record["parent"]["major_minor"] = "259:9"
        elif mode == "wrong_status":
            status = 1
        else:
            raise AssertionError(f"unknown result fixture: {mode}")
        result = json.dumps(record)
    print(result, end="")
    print(os.environ.get("FIXTURE_DOCKER_STDERR", ""), end="", file=sys.stderr)
    return status


class RootFuserTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.uid_map = self.root / "uid_map"
        self.uid_map.write_text("0 0 4294967295\n")
        self.calls = []
        self.fuser_calls = []

    def stat_device(self, path):
        self.calls.append(path)
        devices = {"/dev/nvme1n1p1": (259, 1), "/dev/nvme1n1": (259, 0)}
        major, minor = devices[path]
        return SimpleNamespace(st_mode=stat.S_IFBLK, st_rdev=os.makedev(major, minor))

    def call_check(self, status=1, stderr=b"", stdout=b"", stat_fn=None, euid=0, realpath_fn=None):
        def run_fuser(args, **kwargs):
            self.fuser_calls.append(args)
            return subprocess.CompletedProcess(args, status, stdout, stderr)

        return root_fuser_check.check(
            "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0",
            uid_map_path=self.uid_map, euid_fn=lambda: euid,
            stat_fn=stat_fn or self.stat_device,
            realpath_fn=realpath_fn or (lambda path: path), run_fn=run_fuser,
        )

    def test_real_fuser_recognizes_an_open_file(self):
        # Exercise the host CLI grammar while replacing only the block-node fixtures.
        opened = self.root / "open-target"
        opened.write_text("fixture")
        with opened.open() as held:
            def run_fuser(args, **kwargs):
                return subprocess.run(args[:-2] + [str(opened)], **kwargs)
            status, record = root_fuser_check.check(
                "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0",
                uid_map_path=self.uid_map, euid_fn=lambda: 0,
                stat_fn=self.stat_device, realpath_fn=lambda path: path, run_fn=run_fuser)
            self.assertEqual(status, 1, record)
            self.assertEqual(record["outcome"], "in_use")

    def test_rejects_bad_uid_mapping_or_zero_span_before_device_lookup(self):
        for text in ("0 1000 1\n", "0 0 0\n", "1 0 1\n"):
            self.uid_map.write_text(text)
            status, record = self.call_check()
            self.assertEqual(status, 2)
            self.assertIn("uid_map", record["error"])
            self.assertEqual(record["outcome"], "validation_error")
            self.assertEqual(self.calls, [])
            self.assertEqual(self.fuser_calls, [])

    def test_rejects_missing_nonblock_or_mismatched_device_before_fuser(self):
        for stat_fn in (
            lambda path: (_ for _ in ()).throw(FileNotFoundError(path)),
            lambda path: SimpleNamespace(st_mode=stat.S_IFREG, st_rdev=0),
            lambda path: SimpleNamespace(st_mode=stat.S_IFBLK, st_rdev=os.makedev(259, 9)),
        ):
            status, record = self.call_check(stat_fn=stat_fn)
            self.assertEqual(status, 2)
            self.assertRegex(record["error"], "block|major:minor")
            self.assertEqual(self.fuser_calls, [])

    def test_status_one_requires_empty_output_and_returns_structured_evidence(self):
        status, record = self.call_check()
        self.assertEqual(status, 0)
        self.assertEqual(record["outcome"], "unused")
        self.assertEqual(record["identity"], {
            "effective_uid": 0, "uid_map": [0, 0, 4294967295], "validated": True,
        })
        self.assertEqual(record["target"]["major_minor"], "259:1")
        self.assertEqual(record["parent"]["major_minor"], "259:0")
        self.assertEqual(record["fuser"], {"status": 1, "stdout": "", "stderr": ""})
        root_fuser_check.validate_unused(record, status, "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0")
        for kwargs in ({"stderr": b"lookup failed"}, {"stdout": b"unexpected"}, {"status": 125}):
            status, record = self.call_check(**kwargs)
            self.assertEqual(status, 2)
            self.assertEqual(record["outcome"], "validation_error")
            self.assertEqual(record["fuser"]["stderr"], kwargs.get("stderr", b"").decode())

    def test_in_use_has_distinct_status_and_preserves_fuser_output(self):
        status, record = self.call_check(status=0, stdout=b"1234")
        self.assertEqual(status, 1)
        self.assertEqual(record["outcome"], "in_use")
        self.assertEqual(record["fuser"], {"status": 0, "stdout": "1234", "stderr": ""})
        with self.assertRaises(ValueError):
            root_fuser_check.validate_unused(record, status, "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0")

    def test_nonroot_and_noncanonical_device_fail_before_fuser(self):
        for kwargs in ({"euid": 1000}, {"realpath_fn": lambda path: "/dev/other"}):
            status, record = self.call_check(**kwargs)
            self.assertEqual(status, 2)
            self.assertEqual(record["outcome"], "validation_error")
            self.assertEqual(self.fuser_calls, [])

    def test_host_validator_rejects_incomplete_or_inconsistent_results(self):
        _, valid = self.call_check()
        invalid = [None, [], {}, {**valid, "schema_version": 2}]
        for section, field, value in (
            ("identity", "effective_uid", 1000), ("identity", "validated", False),
            ("identity", "uid_map", [0, 1000, 1]), ("identity", "uid_map", [0, 0, 0]),
            ("identity", "uid_map", None), ("target", "canonical", False),
            ("target", "path", "/dev/other"), ("parent", "is_block_device", False),
            ("fuser", "status", 0), ("fuser", "stdout", "unexpected"),
            ("fuser", "stderr", "lookup failed"),
        ):
            record = copy.deepcopy(valid)
            record[section][field] = value
            invalid.append(record)
        for record in invalid:
            with self.subTest(record=record), self.assertRaises(ValueError):
                root_fuser_check.validate_unused(record, 0, "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0")

    def test_helper_does_not_write_files(self):
        with mock.patch.object(Path, "write_bytes", side_effect=OSError("read-only chroot")):
            status, record = self.call_check()
        self.assertEqual(status, 0)
        self.assertEqual(record["outcome"], "unused")

    def test_real_helper_command_returns_json_without_chroot_writes(self):
        result = subprocess.run(
            [sys.executable, __file__, "--helper-fixture", "run", "--rm", "--privileged",
             "--pid=host", "--userns=host", "--network", "none", "-v", "/:/host:ro",
             "fixture-tools", "chroot", "/host", "/usr/bin/python3",
             str(Path(__file__).with_name("root_fuser_check.py")), "check",
             "/dev/nvme1n1p1", "/dev/nvme1n1", "259:1", "259:0"],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["outcome"], "unused")
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--helper-fixture"]:
        sys.exit(helper_fixture(sys.argv[2:]))
    unittest.main()
