# SPDX-License-Identifier: MPL-2.0

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
RUNNER = SCRIPT_DIR / "run-sweep.sh"
AFFINITY_PATH = SCRIPT_DIR / "qmp_vcpu_affinity.py"


def load_affinity():
    spec = importlib.util.spec_from_file_location("qmp_vcpu_affinity", AFFINITY_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RunnerSafetyTest(unittest.TestCase):
    def test_exit_trap_removes_qemu_after_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "docker.log"
            script = f"""
set -e
docker() {{ printf '%s\\n' \"$*\" >> {log}; }}
export MIXED_SYNC_LIBRARY_ONLY=1
source {RUNNER}
install_qemu_cleanup mixed-sync-test
false
"""
            result = subprocess.run(["bash", "-c", script], check=False)
            actions = log.read_text().splitlines()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rm -f mixed-sync-test", actions)

    def test_vcpu_affinity_excludes_all_iothread_cpus(self) -> None:
        module = load_affinity()
        records = [
            {"cpu-index": cpu, "thread-id": 1000 + cpu}
            for cpu in range(64)
        ]
        assignments = module.assign_vcpu_host_cpus(records, set(range(56, 64)))
        self.assertEqual(len(assignments), 64)
        self.assertEqual({row["guest_cpu"] for row in assignments}, set(range(64)))
        self.assertFalse({row["host_cpu"] for row in assignments} & set(range(56, 64)))
        self.assertEqual(
            [row["host_cpu"] for row in assignments if row["guest_cpu"] < 8],
            list(range(8)),
        )


if __name__ == "__main__":
    unittest.main()
