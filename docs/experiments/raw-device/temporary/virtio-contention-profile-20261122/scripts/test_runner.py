#!/usr/bin/env python3

import unittest
from pathlib import Path


class VirtioForwarderRunnerTest(unittest.TestCase):
    def test_runner_resumes_only_validated_complete_points(self):
        runner = (Path(__file__).parent / "run.sh").read_text()
        self.assertIn('validate_result "$point/result.json" "$expected_bytes"', runner)
        self.assertIn('echo "SKIP validated $case_name"', runner)


if __name__ == "__main__":
    unittest.main()
