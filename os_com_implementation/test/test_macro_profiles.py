#!/usr/bin/env python3
import importlib.util
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PROFILES = [
    "ext2like_sync",
    "ext4_sync",
    "ext2like_nomerge",
    "ext4",
    "ext4_nomerge",
]
LABELS = {
    "ext2like_sync": "ext2-orderless",
    "ext4_sync": "ext4-orderless",
    "ext2like_nomerge": "ext4-rNVPC",
    "ext4": "ext4-rNVPCe",
    "ext4_nomerge": "ext4-nomerge",
}


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class MacroProfileMatrixTest(unittest.TestCase):
    def run_dry(self, relative_script, *args):
        return subprocess.run(
            ["bash", str(ROOT / relative_script), "--dry-run", *args],
            check=True,
            text=True,
            capture_output=True,
        ).stdout

    def test_filebench_five_profile_matrix(self):
        output = self.run_dry(
            "test/filebench-test/run-filebench.sh",
            "--devices", "sata,nvme",
            "--fs", ",".join(PROFILES),
            "--workloads", "write-heavy,fileserver,varmail",
            "--threads", "4,8",
        )
        self.assertIn("Total: 60 rounds", output)
        self.assertIn("runtime=60s timeout=180s", output)
        for profile in PROFILES:
            self.assertEqual(output.count(f"fs={profile} workload="), 12)

    def test_sqlite_five_profile_matrix(self):
        output = self.run_dry(
            "test/sqlite-test/run-sqlite.sh",
            "--devices", "sata,nvme",
            "--fs", ",".join(PROFILES),
            "--sizes", "1000",
            "--runs", "1",
        )
        self.assertIn("Total: 10 rounds", output)
        self.assertIn("timeout=180s", output)
        for profile in PROFILES:
            self.assertEqual(output.count(f"fs={profile} size="), 2)

    def test_parser_labels_cover_five_profiles(self):
        fb = load_module("parse_filebench", ROOT / "test/filebench-test/parse-filebench.py")
        sq = load_module("parse_sqlite", ROOT / "test/sqlite-test/parse-sqlite.py")
        for profile, label in LABELS.items():
            self.assertEqual(fb.FS_LABELS[profile], label)
            self.assertEqual(sq.FS_LABELS[profile], label)

    def test_guest_scripts_define_writeback_profiles(self):
        for relative in (
            "test/filebench-test/guest-run-filebench.sh",
            "test/sqlite-test/guest-run-sqlite.sh",
        ):
            text = (ROOT / relative).read_text()
            self.assertIn("ext2like_sync)", text)
            self.assertIn("ext4_sync)", text)
            self.assertIn("ext2like_nomerge)", text)
            self.assertIn("ext4_nomerge)", text)
            self.assertIn("wb_sync_wait=on", text)
            self.assertIn("wb_merge=off", text)

    def test_harnesses_record_run_configuration(self):
        for relative in (
            "test/filebench-test/run-filebench.sh",
            "test/sqlite-test/run-sqlite.sh",
        ):
            text = (ROOT / relative).read_text()
            self.assertIn("run-config.txt", text)
            self.assertIn("Unknown fs profile: $fs", text)


if __name__ == "__main__":
    unittest.main()
