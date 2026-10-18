#!/usr/bin/env python3
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("aster_ext4_baseline.sh")


class GuestFailureCleanupTest(unittest.TestCase):
    def test_fio_failure_is_reported_after_unmount_cleanup(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        capture = source.index('|| FIO_STATUS=$?')
        status = source.index('=== FIO_EXIT_STATUS: ${FIO_STATUS} ===', capture)
        end = source.index('=== END_FIO_RESULT ===', status)
        unmount = source.index('umount "$MNT"', end)
        exit_status = source.index('exit "$FIO_STATUS"', unmount)

        self.assertLess(capture, status)
        self.assertLess(status, end)
        self.assertLess(end, unmount)
        self.assertLess(unmount, exit_status)


if __name__ == "__main__":
    unittest.main()
