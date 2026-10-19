#!/usr/bin/env python3
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("aster_ext4_baseline.sh")
LAUNCHER = Path(__file__).with_name("run_rnvpc_mixed.sh")


class MixedWorkloadContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = SCRIPT.read_text()

    def test_supported_read_ratios_map_to_randrw(self):
        for ratio in (25, 50, 75):
            with self.subTest(ratio=ratio):
                self.assertIn(f"randrw-r{ratio}-4k)", self.text)
                self.assertIn(f"RWMIXREAD={ratio}", self.text)

    def test_randread_diagnostic_uses_the_prefilled_dataset(self):
        self.assertIn("randread-4k)", self.text)
        self.assertIn("RW=randread", self.text)

    def test_mixed_workload_prefills_directly_outside_measurement(self):
        self.assertIn("prepare-mixed-read-dataset", self.text)
        prepare = self.text.split("prepare-mixed-read-dataset", 1)[1].split("T0=", 1)[0]
        self.assertIn("--direct=1", prepare)
        self.assertNotIn('umount "$MNT"', prepare)

    def test_fio_receives_mix_ratio(self):
        self.assertIn('${RWMIXREAD:+--rwmixread=$RWMIXREAD}', self.text)

    def test_launcher_syncs_harness_and_copies_results_back(self):
        launcher = LAUNCHER.read_text()
        self.assertIn('sync_harness_file "$HERE/aster_ext4_baseline.sh"', launcher)
        self.assertIn('sync_harness_file "$HERE/run-baseline.sh"', launcher)
        self.assertIn("container harness checksum mismatch", launcher)
        self.assertIn('grep -qF randrw-r25-4k', launcher)
        self.assertIn("already uses /dev/nvme3n1p2", launcher)
        self.assertIn('docker cp "$CONTAINER:$CONTAINER_RESULT_DIR/."', launcher)

    def test_runner_propagates_qemu_failures_and_requires_guest_completion(self):
        runner = Path(__file__).with_name("run-baseline.sh").read_text()
        self.assertNotIn('"RNVPC_KCMD=${RNVPC_KCMD}" >> "${ASTER_LOG}" 2>&1 || true', runner)
        self.assertIn("guest did not emit END_FIO_RESULT", runner)


if __name__ == "__main__":
    unittest.main()
