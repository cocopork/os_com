#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import unittest
from pathlib import Path


ROOT = Path(__file__).parent


class RunnerContractTests(unittest.TestCase):
    def test_guest_has_three_exact_modes(self):
        text = (ROOT / "guest.sh").read_text()
        self.assertIn("ext4) OPTIONS=$NATIVE_OPTIONS", text)
        self.assertIn("rnvpc) OPTIONS=delalloc,rnvpc=on,rnvpc_extend=off,rtio=on", text)
        self.assertIn("rnvpce) OPTIONS=delalloc,rnvpc=off,rnvpc_extend=on,rtio=on", text)
        self.assertIn("PRELIM_JOURNAL", text)
        self.assertIn("PRELIM_COMPLETE", text)

    def test_host_enforces_device_and_journal_contract(self):
        text = (ROOT / "run-sweep.sh").read_text()
        self.assertIn('274877906944', text)
        self.assertIn('findmnt -rn -S "$TARGET"', text)
        self.assertIn('^has_journal', text)
        self.assertIn('sweep.py" validate', text)
        self.assertIn('e2fsck -fn', text)


if __name__ == "__main__":
    unittest.main()
