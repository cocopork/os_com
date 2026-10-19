import subprocess
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
LEGACY_HARNESS_FILES = (
    "ext4_journal_compare.sh",
    "ext4_perf_matrix.sh",
    "parse_latency.py",
    "parse_summary.py",
    "run_ext4_journal_compare.sh",
    "run_perf_matrix.sh",
    "run_perf_matrix_img.sh",
    "run_single_round.sh",
)


class ResultsLayoutTests(unittest.TestCase):
    def test_global_results_directory_is_retired(self):
        self.assertFalse((ROOT / "results").exists())

    def test_ext4_compare_harness_lives_under_test(self):
        harness = ROOT / "test" / "ext4-journal-compare"
        for name in LEGACY_HARNESS_FILES:
            self.assertTrue((harness / name).is_file(), name)
            self.assertFalse((ROOT / name).exists(), name)

    def test_macro_harness_owns_its_results(self):
        harness = ROOT / "test" / "macro-tests"
        self.assertTrue((harness / "run-macro-tests.sh").is_file())
        self.assertTrue((harness / "summarize-macro.py").is_file())
        self.assertFalse((ROOT / "test" / "run-macro-tests.sh").exists())
        self.assertFalse((ROOT / "test" / "summarize-macro.py").exists())
        self.assertFalse((ROOT / "test" / "results").exists())
        runner = (harness / "run-macro-tests.sh").read_text(encoding="utf-8")
        self.assertIn("${HARNESS_DIR}/results/", runner)

    def test_dry_runs_do_not_create_result_directories(self):
        cases = (
            (ROOT / "test" / "ext4-journal-compare", "run_ext4_journal_compare.sh"),
            (ROOT / "test" / "ext4-journal-compare", "run_perf_matrix.sh"),
            (ROOT / "test" / "ext4-journal-compare", "run_perf_matrix_img.sh"),
            (ROOT / "test" / "macro-tests", "run-macro-tests.sh"),
        )
        for harness, script in cases:
            results = harness / "results"
            before = set(results.iterdir()) if results.exists() else set()
            subprocess.run(
                ["bash", str(harness / script), "--dry-run"],
                cwd=harness,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
            after = set(results.iterdir()) if results.exists() else set()
            self.assertEqual(before, after, script)


    def test_active_harness_does_not_reference_global_results(self):
        active_roots = (ROOT / "test",)
        forbidden = ("os_com_implementation/results", "${IMPL_DIR_CTR}/results/", "${IMPL_DIR_HOST}/results/")
        for root in active_roots:
            for path in root.rglob("*"):
                if not path.is_file() or path.suffix not in {".sh", ".py"}:
                    continue
                if "archive" in path.parts or "results" in path.parts:
                    continue
                text = path.read_text(encoding="utf-8", errors="replace")
                for marker in forbidden:
                    self.assertNotIn(marker, text, str(path.relative_to(ROOT)))

        harness = ROOT / "test" / "ext4-journal-compare"
        for path in harness.glob("*"):
            if path.suffix not in {".sh", ".py", ".md"}:
                continue
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("os_com_implementation/results", text, path.name)
            self.assertNotIn('${IMPL_DIR_CTR}/results/', text, path.name)
            self.assertNotIn('${IMPL_DIR_HOST}/results/', text, path.name)


if __name__ == "__main__":
    unittest.main()
