#!/usr/bin/env python3
"""Audit Markdown documentation authorities and local references.

Retained experiment documents are deliberately narrow: root or nested
``EXPERIMENTS.md`` files, Markdown below ``evidence/<experiment>/``, and files
marked ``<!-- document-audit: retained-experiment -->``. Each must link to its
package's ``summary.csv``; ordinary documentation is not treated as an experiment.
"""

import re
import sys
from pathlib import Path
from typing import Dict, Iterable, List, Tuple


ALLOWED_STATUS_FILENAMES = {"README.md", "STATUS.md", "EXPERIMENTS.md", "CLEANUP-MANIFEST.md"}
RETAINED_EXPERIMENT_MARKER = "<!-- document-audit: retained-experiment -->"
ZERO_ACCEPTED_EXPERIMENT_RE = re.compile(r"There is no accepted experiment in the current inventory\.", re.IGNORECASE)
ACCEPTED_EVIDENCE_SECTION_RE = re.compile(
    r"^##\s+Accepted Evidence\s*$\n(?P<body>.*?)(?=^##\s|\Z)",
    re.MULTILINE | re.DOTALL,
)
STATUS_HEADING_RE = re.compile(r"^#{1,6}\s*(当前状态|当前进度|下一步)\s*$", re.MULTILINE)
INLINE_LINK_RE = re.compile(r"!?\[[^\]]*\]\(\s*(<[^>\n]+>|[^)\n]*)\)")
REFERENCE_DEFINITION_RE = re.compile(
    r"^\s*\[([^\]]+)\]:\s*(<[^>\n]+>|\S+)(?:[ \t]+.*)?$", re.MULTILINE
)
REFERENCE_LINK_RE = re.compile(r"!?\[([^\]]+)\]\[([^\]]*)\]")
DRAIN_RE = re.compile(r"\bdrain_bw_mib_s\b|drain bandwidth", re.IGNORECASE)
DIAGNOSTIC_RE = re.compile(r"diagnostic|诊断|排空|尾部", re.IGNORECASE)
DRAIN_ASSERTION_RE = re.compile(
    r"(?:\bdrain_bw_mib_s\b|drain bandwidth)"
    r"(?:\s+(?:result|value|measurement|throughput|bandwidth))?"
    r"\s*(?:is|was|=|:)\s*(?:about\s+)?\d",
    re.IGNORECASE,
)
DRAIN_VISUAL_RE = re.compile(r"(?:figure|fig\.?|table|图|表)\s*\d*[^\n]*", re.IGNORECASE)


def markdown_files(root: Path) -> List[Path]:
    return sorted(
        path for path in root.rglob("*.md") if ".git" not in path.parts and ".worktrees" not in path.parts
    )


def visible_markdown(content: str, strip_inline_code: bool = True) -> str:
    """Remove fenced blocks and optionally inline code before applying checks."""
    visible = []
    fence = None
    for line in content.splitlines(keepends=True):
        stripped = line.lstrip()
        marker = stripped[:3]
        if fence is None and marker in ("```", "~~~"):
            fence = marker[0]
            continue
        if fence is not None:
            if stripped.startswith(fence * 3):
                fence = None
            continue
        visible.append(line)
    text = "".join(visible)
    return re.sub(r"`+[^`\n]*`+", "", text) if strip_inline_code else text


def is_archive(path: Path, root: Path) -> bool:
    return "archive" in path.relative_to(root).parts


def is_allowed_status_file(path: Path, root: Path) -> bool:
    return path.name in ALLOWED_STATUS_FILENAMES or is_archive(path, root)


def local_target(destination: str) -> str:
    destination = destination.strip()
    if destination.startswith("<"):
        end = destination.find(">")
        if end != -1:
            return destination[1:end].split("#", maxsplit=1)[0]
    return destination.split(maxsplit=1)[0].split("#", maxsplit=1)[0]


def is_local_target(target: str) -> bool:
    return bool(target) and not target.startswith(("/", "#", "mailto:")) and not re.match(
        r"^[a-z][a-z0-9+.-]*:", target, re.IGNORECASE
    )


def markdown_targets(content: str) -> List[str]:
    """Return inline, image, and referenced local-link destinations."""
    visible = visible_markdown(content)
    definitions = {
        label.casefold(): local_target(destination)
        for label, destination in REFERENCE_DEFINITION_RE.findall(visible)
    }
    targets = [local_target(destination) for destination in INLINE_LINK_RE.findall(visible)]
    for label, reference in REFERENCE_LINK_RE.findall(visible):
        target = definitions.get((reference or label).casefold())
        if target is not None:
            targets.append(target)
    return targets


def link_errors(path: Path, root: Path, content: str) -> Tuple[List[str], List[str]]:
    errors = []
    targets = markdown_targets(content)
    for target in targets:
        if is_local_target(target) and not (path.parent / target).resolve().exists():
            errors.append(f"{path.relative_to(root)}: missing local link target: {target}")
    return errors, targets


def has_explicit_zero_accepted_experiment_state(content: str) -> bool:
    section = ACCEPTED_EVIDENCE_SECTION_RE.search(content)
    if section is None or not ZERO_ACCEPTED_EXPERIMENT_RE.search(section.group("body")):
        return False
    return not any(line.lstrip().startswith(("- ", "* ", "+ ")) for line in section.group("body").splitlines())


def is_retained_experiment_document(path: Path, root: Path, content: str) -> bool:
    relative = path.relative_to(root)
    return (
        (path.name == "EXPERIMENTS.md" and not has_explicit_zero_accepted_experiment_state(content))
        or "evidence" in relative.parts[:-1]
        or any(line.strip() == RETAINED_EXPERIMENT_MARKER for line in content.splitlines())
    )


def has_evidence_path(path: Path, root: Path, targets: Iterable[str]) -> bool:
    document_relative = path.relative_to(root)
    package_summary = None
    if "evidence" in document_relative.parts[:-1]:
        evidence_index = document_relative.parts.index("evidence")
        if len(document_relative.parts) > evidence_index + 2:
            package_summary = root.joinpath(*document_relative.parts[: evidence_index + 2], "summary.csv")
    for target in targets:
        if not is_local_target(target):
            continue
        resolved = (path.parent / target).resolve()
        try:
            relative = resolved.relative_to(root)
        except ValueError:
            continue
        if relative.name != "summary.csv" or "evidence" not in relative.parts:
            continue
        if package_summary is None or resolved == package_summary:
            return True
    return False


def drain_policy_markdown(content: str) -> str:
    text = visible_markdown(content, strip_inline_code=False)
    lines = []
    for line in text.splitlines(keepends=True):
        stripped = line.lstrip()
        if stripped.startswith("#") or "|" in line:
            lines.append(line)
        else:
            lines.append(re.sub(r"`+[^`\n]*`+", "", line))
    return "".join(lines)


def drain_policy_blocks(content: str) -> List[str]:
    paragraphs = [
        paragraph
        for paragraph in re.split(r"\n\s*\n", drain_policy_markdown(content))
        if paragraph.strip()
    ]
    headings: Dict[int, str] = {}
    blocks = []
    document_context = ""
    if paragraphs and paragraphs[0].lstrip().startswith(">") and DIAGNOSTIC_RE.search(paragraphs[0]):
        document_context = paragraphs[0]
    for paragraph in paragraphs:
        heading = re.match(r"^(#{1,6})\s+", paragraph)
        if heading is not None:
            level = len(heading.group(1))
            headings = {key: value for key, value in headings.items() if key < level}
            headings[level] = paragraph
            blocks.append(paragraph)
            continue
        context = ([document_context] if document_context else []) + [headings[level] for level in sorted(headings)]
        blocks.append("\n\n".join(context + [paragraph]))
    return blocks


def is_drain_result_claim(paragraph: str) -> bool:
    if not DRAIN_RE.search(paragraph) or DIAGNOSTIC_RE.search(paragraph):
        return False
    return bool(
        DRAIN_ASSERTION_RE.search(paragraph)
        or ("|" in paragraph and DRAIN_RE.search(paragraph))
        or (DRAIN_VISUAL_RE.search(paragraph) and DRAIN_RE.search(paragraph))
    )


def audit(root: Path) -> Tuple[List[str], Dict[str, int]]:
    root = root.resolve()
    errors: List[str] = []
    files = markdown_files(root)
    for path in files:
        content = path.read_text(encoding="utf-8")
        relative = path.relative_to(root)
        file_errors, targets = link_errors(path, root, content)
        errors.extend(file_errors)
        if is_archive(path, root):
            first_lines = "\n".join(content.splitlines()[:12])
            if not re.search(r"^>\s*(?:\*\*)?Archived:", first_lines, re.MULTILINE):
                errors.append(f"{relative}: archive document lacks an Archived blockquote in its first 12 lines")
        if not is_allowed_status_file(path, root):
            for match in STATUS_HEADING_RE.finditer(visible_markdown(content)):
                errors.append(f"{relative}: current-status heading outside allowlist: {match.group(1)}")
        if is_retained_experiment_document(path, root, content) and not has_evidence_path(path, root, targets):
            errors.append(f"{relative}: retained experiment document lacks an evidence path to evidence/*/summary.csv")
        if any(is_drain_result_claim(block) for block in drain_policy_blocks(content)):
            errors.append(f"{relative}: drain bandwidth formal result claim")
    return errors, {"markdown_files": len(files), "errors": len(errors)}


def main(argv: List[str]) -> int:
    if len(argv) != 2:
        print("usage: document_audit.py <root>", file=sys.stderr)
        return 2
    root = Path(argv[1])
    if not root.is_dir():
        print(f"error: root is not a directory: {root}", file=sys.stderr)
        return 2
    errors, counts = audit(root)
    for error in errors:
        print(error)
    print(f"markdown_files={counts['markdown_files']} errors={counts['errors']}")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
