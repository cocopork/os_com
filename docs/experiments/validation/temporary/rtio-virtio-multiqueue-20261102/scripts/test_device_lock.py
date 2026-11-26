#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Exercise process-level serialization without a device, Docker or QEMU."""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('run-validation.sh')


class DeviceLockTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.roots = [Path(self.temp.name) / name for name in ('root-a', 'root-b')]
        for root in self.roots:
            root.mkdir()
        self.identity = f'999999:{os.getpid()}'
        self.other = f'999998:{os.getpid()}'
        self.addCleanup(self.remove_lock_files)

    def remove_lock_files(self):
        for identity in (self.identity, self.other):
            Path('/run/lock', 'rtio-mq-' + identity.replace(':', '-') + '.lock').unlink(missing_ok=True)

    def command(self, body, *, identity=None, root=1):
        return ['bash', '-c', 'set -euo pipefail; source "$1"; ART=$3; ROOT_ART=$3; ' + body,
                'fixture', str(SCRIPT), self.identity if identity is None else identity, str(self.roots[root])]

    def hold(self):
        process = subprocess.Popen(self.command('acquire_device_lock "$2"; echo locked; read -r release', root=0),
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.close_process, process)
        message = process.stdout.readline().strip()
        if message != 'locked':
            self.fail('lock holder did not start: ' + process.stderr.read())
        return process

    @staticmethod
    def close_process(process):
        if process.poll() is None:
            process.communicate('release\n', timeout=5)
        else:
            process.communicate(timeout=5)

    def test_same_device_different_roots_exclude_and_release(self):
        held = self.hold()
        attempt = subprocess.run(self.command('acquire_device_lock "$2"; echo destructive'), capture_output=True, text=True)
        self.assertEqual(attempt.returncode, 2, attempt.stderr)
        self.assertNotIn('destructive', attempt.stdout)
        held.communicate('release\n', timeout=5)
        attempt = subprocess.run(self.command('acquire_device_lock "$2"; echo destructive'), capture_output=True, text=True)
        self.assertEqual(attempt.returncode, 0, attempt.stderr)
        self.assertIn('destructive', attempt.stdout)

    def test_standalone_runner_acquires_lock_before_creating_artifacts(self):
        inputs = Path(self.temp.name) / 'inputs'
        inputs.mkdir()
        for name in ('selected.iso', 'ext2.img', 'exfat.img'):
            (inputs / name).touch()
        body = r'''
            SOURCE_ART=$3/../inputs
            python3() {
                if [[ $2 == target-preflight ]]; then
                    printf '{"target_major_minor":"%s","parent_major_minor":"999997:0","parent":"nvme1n1"}\n' "$fixture_identity"
                else
                    command python3 "$@"
                fi
            }
            docker() { echo 'Docker must not run in this fixture' >&2; exit 99; }
            create_artifact() { echo locked; read -r release; exit 0; }
            fixture_identity=$2
            main --authorized-destructive-target=/dev/nvme1n1p1
        '''
        held = subprocess.Popen(self.command(body, root=0), stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.close_process, held)
        self.assertEqual(held.stdout.readline().strip(), 'locked')
        contender = subprocess.run(self.command(body), input='release\n', capture_output=True, text=True)
        self.assertEqual(contender.returncode, 2, contender.stderr)
        self.assertNotIn('locked', contender.stdout)
        held.communicate('release\n', timeout=5)

    def test_distinct_verified_devices_do_not_share_lock(self):
        self.hold()
        attempt = subprocess.run(self.command('acquire_device_lock "$2"; echo acquired', identity=self.other),
                                 capture_output=True, text=True)
        self.assertEqual(attempt.returncode, 0, attempt.stderr)
        self.assertIn('acquired', attempt.stdout)

    def test_post_guest_fsck_keeps_device_locked(self):
        body = '''
            acquire_device_lock "$2"
            TARGET=/dev/fixture
            confirm_target_idle() { echo checked >> "$ART/events"; }
            TOOLS_IMAGE=fixture
            docker() {
                [[ "$*" == 'run --rm --network none --device /dev/fixture:/dev/benchmark-target:r fixture e2fsck -fn /dev/benchmark-target' ]]
                local status=0
                bash -c 'source "$1"; acquire_device_lock "$2"' fixture "$SCRIPT_DIR/run-validation.sh" "$fixture_identity" || status=$?
                [[ $status == 2 ]] || return 99
                echo fsck >> "$ART/events"
            }
            fixture_identity=$2
            check_filesystem
            [[ $(cat "$ART/e2fsck.status") == 0 ]]
            [[ $(cat "$ART/events") == $'checked\\nfsck' ]]
        '''
        attempt = subprocess.run(self.command(body), capture_output=True, text=True)
        self.assertEqual(attempt.returncode, 0, attempt.stderr)

    def test_malformed_identity_rejected_without_opening_lock(self):
        for identity in ('', '../escape', '259:6/extra'):
            attempt = subprocess.run(self.command('acquire_device_lock "$2"', identity=identity),
                                     capture_output=True, text=True)
            self.assertEqual(attempt.returncode, 2, attempt.stderr)

if __name__ == '__main__':
    unittest.main()
