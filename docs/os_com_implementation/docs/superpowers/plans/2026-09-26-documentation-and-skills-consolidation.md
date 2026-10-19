# os_com/AchillesFS Documentation and Skills Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert `os_com_implementation` into a small authoritative documentation set, retain only conclusion-bearing experiment evidence, and create reusable project-local filesystem development skills.

**Architecture:** Establish four root documents as the only current truth, reduce topic documents to bounded references, and move superseded but valuable material into marked archives. Build a cleanup manifest before any deletion. Split reusable guidance into a generic filesystem skill and an Asterinas/EXT4/AchillesFS specialization that reads live project status rather than embedding it.

**Tech Stack:** Markdown, Git, shell/`rg`/`find`/`du`, Python 3 standard library for link and asset validation, Codex project-local skills.

**Spec:** `docs/superpowers/specs/2026-09-26-documentation-and-skills-consolidation-design.md`

## Global Constraints

- Do not modify `os_com` kernel behavior or filesystem semantics.
- Preserve unrelated working-tree changes; inspect every dirty file before editing and stage only task-owned paths.
- `STATUS.md` is the sole current progress source.
- Formal throughput results use `busy_bw_mib_s`; drain bandwidth is diagnostic only.
- Do not alter numerical values in retained final CSV files.
- Do not delete any artifact referenced by a retained document, final plot, or accepted conclusion.
- Create and review `CLEANUP-MANIFEST.md` before destructive cleanup; deletion requires the user's explicit approval of that concrete manifest.
- Do not download containers, packages, or large external data.

---

### Task 1: Build the Fact Inventory and Validation Tool

**Files:**
- Create: `tools/document_audit.py`
- Create: `tools/test_document_audit.py`
- Create: `STATUS.md`
- Read: `../os_com/kernel/src/fs/fs_impls/ext4/`
- Read: `../os_com/kernel/src/fs/rnvpc/`
- Read: `README.md`
- Read: `NEXT-SESSION.md`
- Read: `os_com_refactor_plan/*/README.md`
- Read: `rNVPC-rTIO/**/*.md`

**Interfaces:**
- Consumes: current `os_com` branch, commit history, source symbols, documentation headings, result summaries.
- Produces: `tools/document_audit.py ROOT`, returning nonzero for broken local Markdown links, duplicate current-status declarations outside allowed files, retained experiment documents without evidence paths, or formal use of `drain_bw_mib_s`; `STATUS.md` becomes the progress authority consumed by later tasks and the project skill.

- [ ] **Step 1: Write audit tests before the validator**

Create standard-library `unittest` cases using a temporary document tree. Cover:

```python
def test_rejects_broken_local_markdown_link(): ...
def test_rejects_current_status_section_outside_allowlist(): ...
def test_rejects_formal_drain_bandwidth_claim(): ...
def test_accepts_archive_status_and_drain_diagnostics(): ...
def test_accepts_busy_bandwidth_formal_result(): ...
```

The allowlist is `README.md`, `STATUS.md`, `EXPERIMENTS.md`, `CLEANUP-MANIFEST.md`, and files below `archive/`. Archive files may describe historical status but must contain an archive banner.

- [ ] **Step 2: Run tests and verify the validator is absent**

Run: `python3 -m unittest tools.test_document_audit -v`

Expected: FAIL because `tools.document_audit` does not exist.

- [ ] **Step 3: Implement the audit utility**

Implement a dependency-free CLI with these checks:

```text
document_audit.py <root>
  scan *.md excluding .git/.worktrees
  resolve relative Markdown links and report missing local targets
  reject headings matching 当前状态|当前进度|下一步 outside allowlist/archive
  reject drain_bw_mib_s or "drain bandwidth" in non-diagnostic result claims
  require archive documents to begin with an Archived blockquote in first 12 lines
  print one error per line and exit 1; print counts and exit 0 otherwise
```

Diagnostic uses are accepted only when the containing paragraph includes `diagnostic`, `诊断`, `排空`, or `尾部`.

- [ ] **Step 4: Run validator unit tests**

Run: `python3 -m unittest tools.test_document_audit -v`

Expected: all five tests PASS.

- [ ] **Step 5: Audit current implementation facts and write `STATUS.md`**

Use source and Git evidence, not old progress prose. Record:

```markdown
# Current Status
## Scope And Repositories
## Code Baseline
## Completed Mechanisms
## Experiment Progress
## In Progress
## Known Blockers And Limitations
## Next Tasks
## Verification Evidence
```

Every mechanism row must contain `Status`, `Code evidence`, `Validation`, and `Residual risk`. Mark uncertain claims `Unverified`; never infer completion from a plan checkbox.

- [ ] **Step 6: Commit the inventory foundation**

```bash
git add tools/document_audit.py tools/test_document_audit.py STATUS.md
git commit -m "docs: establish implementation status authority"
```

---

### Task 2: Rewrite the Root Documentation Set

**Files:**
- Modify: `README.md`
- Create: `EXPERIMENTS.md`
- Create: `LESSONS.md`
- Archive then remove as root authority: `NEXT-SESSION.md`
- Read: `ext4_performance.md`
- Read: `ext4-function-check.md`
- Read: `ext4_ktest_design_principle.md`

**Interfaces:**
- Consumes: verified `STATUS.md`, current dirty `README.md` diff, accepted result summaries.
- Produces: root navigation and stable summaries used by all reduced topic documents and both skills.

- [ ] **Step 1: Protect existing README edits**

Run and save for review:

```bash
git diff -- README.md
git show HEAD:README.md > /tmp/os-com-implementation-readme-head.md
```

Extract still-valid five-profile configuration changes from the working copy before rewriting. Do not restore the HEAD version over user changes.

- [ ] **Step 2: Rewrite `README.md` as a navigation document**

Keep it under 250 lines with exactly these responsibilities:

```markdown
# os_com EXT4 / AchillesFS Research Implementation
## What This Repository Contains
## Current Entry Points
## Repository Relationships
## Quick Validation
## Experiment Safety
## Archive Policy
```

Link to `STATUS.md`, `EXPERIMENTS.md`, `LESSONS.md`, the AchillesFS design, refactor task table, and test index. Move mutable progress and long performance discussion out.

- [ ] **Step 3: Create `EXPERIMENTS.md`**

For each accepted experiment, include: purpose, compared objects, exact workload, device/environment, code revision, instrumentation state, accepted `busy_bw_mib_s` result path, conclusion, and limitation. Separate `Accepted Evidence`, `Diagnostic Evidence`, and `Pending Experiments`.

- [ ] **Step 4: Create `LESSONS.md`**

Condense only verified lessons under:

```markdown
## Correctness And Persistence
## Extent And Allocation
## PageCache And Writeback
## BIO And Completion Context
## Virtualization And Devices
## Experiment Methodology
```

Each lesson uses `Symptom`, `Root cause`, `Rule`, and `Evidence`; links carry details instead of copied chronology.

- [ ] **Step 5: Archive `NEXT-SESSION.md` content**

Move durable historical material to `archive/debugging/2026-09-24-session-handoff.md`, prepend:

```markdown
> Archived: 2026-09-26. Historical handoff only. Current progress lives in ../../STATUS.md.
```

Remove obsolete command/status duplication and delete the root `NEXT-SESSION.md` path through `git mv` plus edit, so Git preserves history.

- [ ] **Step 6: Validate and commit root documents**

Run:

```bash
python3 tools/document_audit.py .
rg -n '^## (当前状态|当前进度|下一步)' --glob '*.md' --glob '!archive/**'
```

Expected: validator PASS; mutable status appears only in allowed root documents.

Commit:

```bash
git add README.md STATUS.md EXPERIMENTS.md LESSONS.md NEXT-SESSION.md archive/debugging/2026-09-24-session-handoff.md
git commit -m "docs: consolidate project status and experiment evidence"
```

---

### Task 3: Compress Refactor and AchillesFS Design Documents

**Files:**
- Modify: `os_com_refactor_plan/README.md`
- Modify: `os_com_refactor_plan/001-pagecache-dirty-index/README.md` through `011-nojournal-vs-journal-writeback/README.md`
- Modify: `rNVPC-rTIO/README.md`
- Create: `rNVPC-rTIO/design.md`
- Create: `rNVPC-rTIO/implementation.md`
- Archive: superseded documents from `rNVPC-rTIO/` and `os_com_refactor_plan/010-nvpool-port/README.md`
- Archive: superseded but methodologically useful experiment narratives under `archive/experiments/`

**Interfaces:**
- Consumes: `STATUS.md`, source evidence, existing design evolution and task documents.
- Produces: bounded task references and one authoritative AchillesFS mechanism design.

- [ ] **Step 1: Replace the refactor index with a status table**

Use columns `ID`, `Mechanism`, `Current status`, `Code evidence`, `Validation`, `Remaining work`, `Detail`. Status values are limited to `Complete`, `Partial`, `Planned`, `Superseded`, `Blocked` and must match `STATUS.md`.

- [ ] **Step 2: Reduce each task README**

Keep each under 300 lines and use:

```markdown
# <ID> <Name>
## Goal
## Current Status
## Implemented Result
## Verification Evidence
## Remaining Work
## Historical Notes
```

Move chronological trial logs and rejected hypotheses with lasting value into `archive/debugging/<id>-<topic>.md`; delete duplicated raw tables after their accepted summary is linked.

- [ ] **Step 3: Merge AchillesFS design authority**

Create `rNVPC-rTIO/design.md` with mechanism boundaries, persistent/volatile objects, write/read/writeback flows, soft-update ordering, completion context, recovery model, and invariants. Explicitly distinguish implemented behavior from intended behavior using links to `STATUS.md` rather than inline progress claims.

- [ ] **Step 4: Create AchillesFS implementation reference**

Create `rNVPC-rTIO/implementation.md` with module map, mount/config switches, state transitions, thread ownership, test hooks, known limitations, and current validation commands. Do not duplicate experiment numbers from `EXPERIMENTS.md`.

- [ ] **Step 5: Archive superseded design documents**

Move superseded design evolution, porting, per-stage implementation notes, and the old NVPool plan into `archive/design/`. Add archive banners and a `Replaced by` link to `rNVPC-rTIO/design.md` or `implementation.md`. Keep focused debugging studies only when cited by `LESSONS.md` or `EXPERIMENTS.md`.

Move experiment narratives whose conclusions were replaced but whose methodology remains useful into `archive/experiments/`. Their banners must name the accepted replacement evidence; raw logs and unaccepted intermediate datasets are cleanup candidates, not archive material.

- [ ] **Step 6: Validate and commit design consolidation**

Run:

```bash
python3 tools/document_audit.py .
find os_com_refactor_plan rNVPC-rTIO -name '*.md' -printf '%s %p\n' | sort -nr
```

Expected: no broken links/status duplication; authoritative files are visibly shorter than the pre-task inventory.

Commit all task-owned paths with:

```bash
git add os_com_refactor_plan rNVPC-rTIO archive/design archive/experiments archive/debugging
git commit -m "docs: compress refactor and AchillesFS design history"
```

---

### Task 4: Normalize Test Documentation and Accepted Evidence

**Files:**
- Create: `test/README.md`
- Modify: `test/aster-ext4-baseline-test/README.md`
- Modify: `test/filebench-test/README.md`
- Modify: `test/sqlite-test/README.md`
- Create/move: `evidence/<accepted-experiment>/{README.md,summary.csv,plot.py,figure.*}`
- Modify: `EXPERIMENTS.md`

**Interfaces:**
- Consumes: accepted plot inputs and summaries identified by `EXPERIMENTS.md`; existing dirty test README changes must be merged, not overwritten.
- Produces: self-contained accepted evidence directories and minimal reproducible harness documentation.

- [ ] **Step 1: Inventory references before moving data**

Generate `/tmp/os-com-evidence-references.txt` with:

```bash
rg -n 'results/|plots/|summary\.csv|\.png|\.pdf' --glob '*.md' --glob '*.py' . > /tmp/os-com-evidence-references.txt
```

For every candidate accepted dataset, record all callers before `git mv` or copying.

- [ ] **Step 2: Write `test/README.md`**

Document shared KVM/container requirements, compilation reuse, device ownership, image isolation, latency switch policy, completion markers, destructive formatting warnings, and result acceptance rules. Link specialized harnesses rather than repeating their commands.

- [ ] **Step 3: Reduce each harness README**

Each README keeps `Purpose`, `Matrix`, `Prerequisites`, `Command`, `Output`, `Acceptance checks`, and `Known limitations`. Preserve current uncommitted filebench/SQLite corrections after reviewing their diffs.

- [ ] **Step 4: Create accepted evidence packages**

For each experiment referenced by the current paper/design, copy or move only the final summary, final plotting source, final figure, and a concise README. The README must identify the source result directory and state that formal plots read `busy_bw_mib_s`.

- [ ] **Step 5: Verify numerical preservation**

Before and after each move:

```bash
sha256sum <old-summary.csv> <new-summary.csv>
```

Expected: identical hashes for retained CSV data. Update links only after hash equality is recorded in the evidence README.

- [ ] **Step 6: Run harness and documentation checks**

Run existing non-destructive tests:

```bash
python3 test/test_macro_profiles.py
python3 test/aster-ext4-baseline-test/test_mixed_workloads.py
python3 test/aster-ext4-baseline-test/test_parse_mixed.py
python3 test/aster-ext4-baseline-test/test_parse_remount_recovery.py
python3 test/aster-ext4-baseline-test/test_plot_baseline.py
python3 test/aster-ext4-baseline-test/test_plot_motivation.py
python3 test/aster-ext4-baseline-test/test_plot_writeback_matrix.py
python3 test/recovery-test/test_plot_recovery.py
python3 tools/document_audit.py .
```

- [ ] **Step 7: Commit normalized test documentation and evidence**

Stage only reviewed test documentation, evidence packages, link updates, and already-present harness work that belongs to the accepted experiments. Use:

```bash
git commit -m "docs: normalize test evidence and reproduction guides"
```

---

### Task 5: Produce and Execute the Cleanup Manifest

**Files:**
- Create: `CLEANUP-MANIFEST.md`
- Delete after approval: ignored/incomplete result directories, transient logs, caches, rejected intermediate data.
- Modify after deletion: `EXPERIMENTS.md`

**Interfaces:**
- Consumes: accepted evidence packages and all remaining artifact references.
- Produces: a user-approved, auditable deletion boundary and before/after storage report.

- [ ] **Step 1: Record pre-cleanup metrics**

Capture in the manifest:

```bash
find . -path './.git' -prune -o -path './.worktrees' -prune -o -type f -print | wc -l
du -sh --exclude=.git --exclude=.worktrees .
find results test ktest-logs -type f 2>/dev/null | wc -l
```

- [ ] **Step 2: Classify every cleanup candidate**

Use a table with `Path`, `Class`, `Decision`, `Reason`, `Retained replacement`, `Referenced by`, and `Size`. Allowed decisions are `Keep`, `Archive`, and `Delete`. No wildcard-only row may authorize deletion; list directories or file groups with an exact generated file list attached in fenced text.

- [ ] **Step 3: Validate the proposed deletion set**

For every `Delete` path, require all checks to return no retained references:

```bash
git grep -n -- '<path-or-basename>' -- '*.md' '*.py' '*.sh'
rg -n '<path-or-basename>' README.md STATUS.md EXPERIMENTS.md LESSONS.md evidence archive
```

Tracked source and harness files cannot be classified `Delete` merely because they are currently unused.

- [ ] **Step 4: Commit the manifest and stop for explicit approval**

```bash
git add CLEANUP-MANIFEST.md
git commit -m "docs: inventory disposable experiment artifacts"
```

Present counts and estimated reclaimed space. Do not execute deletion until the user approves this exact manifest.

- [ ] **Step 5: Delete only approved paths**

After approval, remove the exact listed paths. Do not use broad globs beyond the manifest's expanded list. Remove empty parent directories only when they contain no retained file.

- [ ] **Step 6: Record post-cleanup metrics and verify references**

Repeat Step 1 metrics, append actual reclaimed space, and run:

```bash
python3 tools/document_audit.py .
git status --short
```

- [ ] **Step 7: Commit cleanup results**

```bash
git add -A -- CLEANUP-MANIFEST.md results test ktest-logs archive evidence EXPERIMENTS.md
git commit -m "chore: remove obsolete experiment artifacts"
```

---

### Task 6: Create the Two Project-Local Skills

**Files:**
- Create: `.codex/skills/filesystem-development/SKILL.md`
- Create: `.codex/skills/filesystem-development/agents/openai.yaml`
- Create: `.codex/skills/filesystem-development/references/correctness.md`
- Create: `.codex/skills/filesystem-development/references/performance.md`
- Create: `.codex/skills/filesystem-development/references/experiments.md`
- Create: `.codex/skills/asterinas-ext4-achillesfs/SKILL.md`
- Create: `.codex/skills/asterinas-ext4-achillesfs/agents/openai.yaml`
- Create: `.codex/skills/asterinas-ext4-achillesfs/references/architecture.md`
- Create: `.codex/skills/asterinas-ext4-achillesfs/references/environment.md`
- Create: `.codex/skills/asterinas-ext4-achillesfs/references/known-issues.md`

**Interfaces:**
- Consumes: `STATUS.md`, `EXPERIMENTS.md`, `LESSONS.md`, authoritative AchillesFS design, and test README.
- Produces: two automatically discoverable skills; the specialization routes to the generic skill and live project documents.

- [ ] **Step 1: Initialize both skills**

Run:

```bash
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/init_skill.py filesystem-development --path .codex/skills --resources references
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/init_skill.py asterinas-ext4-achillesfs --path .codex/skills --resources references
```

- [ ] **Step 2: Write the generic skill entrypoint**

Its description targets filesystem mechanism design, correctness debugging, I/O performance analysis, and experiment planning. The body routes among mechanism design, correctness, performance, and experiments. It states that fault model/persistence authority is defined or updated only for new or semantically changed mechanisms, not for every implementation edit.

- [ ] **Step 3: Write generic references**

- `correctness.md`: authority, state-machine invariants, lock order, completion context, re-dirty/ABA, crash validation.
- `performance.md`: memory/allocation/BIO/submission/completion/post-I/O decomposition and anti-patterns.
- `experiments.md`: device limits, cache/workset sizing, instrumentation, repeats, acceptance markers, and formal `busy_bw_mib_s` use.

- [ ] **Step 4: Write the project specialization entrypoint**

Require this read order when applicable:

```text
1. ../filesystem-development/SKILL.md
2. ../../../STATUS.md
3. ../../../EXPERIMENTS.md
4. ../../../rNVPC-rTIO/design.md or implementation.md as needed
```

The skill must not copy current task completion or performance numbers.

- [ ] **Step 5: Write project references**

- `architecture.md`: repositories, EXT4/JBD2/PageCache/extent/rNVPCe/rTIO ownership and persistent versus volatile state.
- `environment.md`: container, KVM, QEMU, virtio-blk, test devices/images, one-build matrix, and resource isolation.
- `known-issues.md`: completion callbacks, dirty marks, flatten/rebuild, BIO segmentation, writeback backpressure, stale container harnesses, missing completion markers, and instrumentation distortion; each links to evidence.

- [ ] **Step 6: Generate and validate skill metadata**

Run:

```bash
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/quick_validate.py .codex/skills/filesystem-development
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/quick_validate.py .codex/skills/asterinas-ext4-achillesfs
```

Then verify no project paths leak into the generic skill:

```bash
! rg -n 'os_com|Achilles|Asterinas|/home/zn' .codex/skills/filesystem-development
! rg -n 'MiB/s|2026[0-9]{4}|Complete|Partial' .codex/skills/asterinas-ext4-achillesfs/SKILL.md
```

- [ ] **Step 7: Forward-test routing behavior**

Use two read-only prompts in isolated `/tmp` output locations:

```text
Generic: Diagnose a filesystem whose BIO completion callback acquires an allocator lock.
Project: Locate the authoritative current AchillesFS writeback state and propose the correct validation command.
```

Expected: generic response applies interrupt/completion-context reasoning without project paths; project response reads `STATUS.md` rather than quoting embedded progress.

- [ ] **Step 8: Commit skills**

```bash
git add .codex/skills
git commit -m "docs: add filesystem and AchillesFS development skills"
```

---

### Task 7: Final Consistency and Handoff Verification

**Files:**
- Modify: `README.md`
- Modify: `STATUS.md`
- Modify: `EXPERIMENTS.md`
- Modify: `CLEANUP-MANIFEST.md`

**Interfaces:**
- Consumes: all completed tasks.
- Produces: final navigation, verification report, and clean documentation handoff.

- [ ] **Step 1: Run the full documentation and skill gate**

```bash
python3 -m unittest tools.test_document_audit -v
python3 tools/document_audit.py .
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/quick_validate.py .codex/skills/filesystem-development
python3 /home/zn/.codex/skills/.system/skill-creator/scripts/quick_validate.py .codex/skills/asterinas-ext4-achillesfs
```

- [ ] **Step 2: Run retained harness unit tests**

Run every existing Python/shell static test retained under `test/`, including plot regeneration tests. Do not launch destructive device tests; this task validates documentation and retained tooling, not new performance data.

- [ ] **Step 3: Verify navigation depth manually**

Starting from `README.md`, demonstrate paths of at most three links to:

```text
current progress
AchillesFS authoritative design
accepted performance conclusion and CSV
one-command test reproduction
generic filesystem development skill
project-specific skill
```

Record these paths in the README's `Current Entry Points` table.

- [ ] **Step 4: Verify status uniqueness and busy-bandwidth policy**

```bash
rg -n '^#{1,4} .*当前(状态|进度)|^#{1,4} .*下一步' --glob '*.md' --glob '!archive/**' .
rg -n 'drain_bw_mib_s|drain bandwidth' README.md STATUS.md EXPERIMENTS.md evidence .codex/skills
rg -n 'busy_bw_mib_s' EXPERIMENTS.md evidence .codex/skills/filesystem-development/references/experiments.md
```

Expected: status headings only in allowed authority files; drain references are diagnostic; formal evidence and skill guidance use busy bandwidth.

- [ ] **Step 5: Review final diff and commit handoff updates**

```bash
git diff --check
git status --short
git diff --stat
git add README.md STATUS.md EXPERIMENTS.md CLEANUP-MANIFEST.md
git commit -m "docs: finalize AchillesFS implementation knowledge base"
```

- [ ] **Step 6: Report completion evidence**

Report commits, retained evidence packages, deleted artifact count and bytes, validator/test results, and any unresolved `Unverified` status items. Do not claim code correctness or performance beyond the retained evidence.
