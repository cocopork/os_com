import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from tools.document_audit import audit


class DocumentAuditTests(unittest.TestCase):
    def write(self, root: Path, relative: str, content: str) -> None:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def test_rejects_broken_local_markdown_link(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "README.md", "[missing](missing.md)\n")
            errors, _ = audit(root)
            self.assertTrue(any("missing local link target" in error for error in errors))

    def test_rejects_current_status_section_outside_allowlist(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "notes.md", "## 当前状态\n")
            errors, _ = audit(root)
            self.assertTrue(any("current-status heading" in error for error in errors))

    def test_rejects_formal_drain_bandwidth_claim(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "results.md", "The drain_bw_mib_s result is 12.0.\n")
            errors, _ = audit(root)
            self.assertTrue(any("drain bandwidth formal result" in error for error in errors))

    def test_rejects_numeric_drain_result_despite_policy_prose(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(
                root,
                "policy.md",
                "Policy: do not use drain bandwidth for formal results. "
                "Table 1 reports drain bandwidth: 123 MiB/s.\n",
            )
            errors, _ = audit(root)
            self.assertTrue(any("drain bandwidth formal result" in error for error in errors))

    def test_rejects_inline_code_drain_metric_heading_for_result_table(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(
                root,
                "RESULTS.md",
                "## Throughput (`drain_bw_mib_s`)\n\n"
                "### SATA random write\n\n"
                "| workload | MiB/s |\n"
                "| --- | ---: |\n"
                "| ext4 | 401 |\n",
            )
            errors, _ = audit(root)
            self.assertTrue(any("drain bandwidth formal result" in error for error in errors))

    def test_accepts_inline_code_drain_table_under_diagnostic_banner(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(
                root,
                "RESULTS.md",
                "> Historical diagnostic evidence only; not a formal result.\n\n"
                "# Results\n\n"
                "## Throughput (`drain_bw_mib_s`)\n\n"
                "| workload | MiB/s |\n"
                "| --- | ---: |\n"
                "| ext4 | 401 |\n",
            )
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_accepts_archive_status_and_drain_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "archive/history.md", "> **Archived:** original date.\n\n## 当前状态\n\nThe drain bandwidth diagnostic covers tail drain behavior.\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_accepts_busy_bandwidth_formal_result(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "RESULTS.md", "The busy_bw_mib_s result is 12.0.\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_accepts_explicit_zero_accepted_experiment_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(
                root,
                "EXPERIMENTS.md",
                "## Accepted Evidence\n\nThere is no accepted experiment in the current inventory.\n",
            )
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_rejects_nonempty_accepted_evidence_without_package_link(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(
                root,
                "EXPERIMENTS.md",
                "## Accepted Evidence\n\nThere is no accepted experiment in the current inventory.\n\n- Accepted baseline result.\n",
            )
            errors, _ = audit(root)
            self.assertTrue(any("retained experiment document lacks an evidence path" in error for error in errors))

    def test_rejects_retained_experiment_document_without_evidence_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "EXPERIMENTS.md", "# Experiments\n\nRetained result.\n")
            errors, _ = audit(root)
            self.assertTrue(any("retained experiment document lacks an evidence path" in error for error in errors))

    def test_accepts_retained_experiment_document_with_evidence_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "evidence/write/summary.csv", "busy_bw_mib_s\n12.0\n")
            self.write(root, "EXPERIMENTS.md", "[write evidence](evidence/write/summary.csv)\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_rejects_evidence_package_readme_linking_other_summary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "evidence/first/summary.csv", "own\n")
            self.write(root, "evidence/second/summary.csv", "other\n")
            self.write(root, "evidence/first/README.md", "[wrong](../second/summary.csv)\n")
            errors, _ = audit(root)
            self.assertTrue(any("retained experiment document lacks an evidence path" in error for error in errors))

    def test_accepts_evidence_package_readme_linking_own_summary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "evidence/first/summary.csv", "own\n")
            self.write(root, "evidence/first/README.md", "[summary](summary.csv)\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_accepts_policy_prose_about_drain_bandwidth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "policy.md", "The policy says formal results must not use drain bandwidth; documentation names `<!-- document-audit: retained-experiment -->`.\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_ignores_links_in_code_spans_and_content_in_fences(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "notes.md", "Use `[example](missing.md)`.\n\n```text\n[example](also-missing.md)\ndrain bandwidth: 12 MiB/s\n```\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_checks_reference_style_links_and_image_destinations(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "README.md", "[guide][guide-ref]\n![chart][chart-ref]\n\n[guide-ref]: docs/guide.md\n[chart-ref]: images/chart.png\n")
            errors, _ = audit(root)
            self.assertEqual(2, sum("missing local link target" in error for error in errors))

    def test_accepts_angle_bracket_destination_with_title(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "sample doc.md", "ok\n")
            self.write(root, "README.md", "[sample](<sample doc.md> \"title\")\n")
            errors, _ = audit(root)
            self.assertEqual([], errors)

    def test_rejects_archive_document_without_banner(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "archive/history.md", "# Historical record\n")
            errors, _ = audit(root)
            self.assertTrue(any("archive document lacks an Archived blockquote" in error for error in errors))

    def test_cli_reports_success_and_failure_counts(self) -> None:
        tool = Path(__file__).with_name("document_audit.py")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "README.md", "# Clean\n")
            success = subprocess.run([sys.executable, str(tool), str(root)], text=True, capture_output=True, check=False)
            self.assertEqual(0, success.returncode)
            self.assertIn("markdown_files=1 errors=0", success.stdout)
            self.write(root, "README.md", "[missing](missing.md)\n")
            failure = subprocess.run([sys.executable, str(tool), str(root)], text=True, capture_output=True, check=False)
            self.assertEqual(1, failure.returncode)
            self.assertIn("missing local link target", failure.stdout)
            self.assertIn("markdown_files=1 errors=1", failure.stdout)


if __name__ == "__main__":
    unittest.main()
