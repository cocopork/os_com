# Cleanup Manifest

> Status: completed. The explicitly approved C1 cache deletion, global `results/` retirement, and root diagnostic-log cleanup ran on 2026-09-27.
> C1 removed only its 25 exact paths. Separate user approvals removed the complete legacy global `results/` directory after active producers moved under `test/ext4-journal-compare/`, then removed the untracked root-level `i5-*.log` and `smoke-*.log` diagnostics.

## Scope and pre-cleanup metrics

This manifest inventories disposable experiment artifacts in the working tree
at `docs-consolidation-20260927`. It preserves all tracked and untracked source,
harness, and test files, including the current user-owned dirty files reported
by `git status --short`. The reproducibility snapshot below was taken after
commit `b28788d` and before any deletion. It includes this committed manifest
and the ignored, uncommitted Task 5 report. The earlier `1,285` count preceded
creation of those two files; this snapshot's `1,287` count is the approval
baseline.

The following required commands were run before this manifest was created:

```text
find . -path './.git' -prune -o -path './.worktrees' -prune -o -type f -print | wc -l
1287

du -sh --exclude=.git --exclude=.worktrees .
63M	.

find results test ktest-logs -type f 2>/dev/null | wc -l
1152
```

There are no accepted evidence packages: `EXPERIMENTS.md` states that no
experiment has the required summary, completion marker, configuration, and
provenance. That status means historical outputs cannot support a formal result,
but it does not make them safe to delete without an explicit archival decision.

## Candidate classification

`Candidate groups` counts the rows below. `Candidate files` counts the
constituent regular files in those groups; the table excludes source, harness,
and test programs. `Archive` is a retention decision for later curated archival,
not an instruction to move files in this task.

| Path | Class | Decision | Reason | Retained replacement | Referenced by | Size |
| --- | --- | --- | --- | --- | --- | --- |
| `results/` (872 files) | Historical benchmark results, raw QEMU/serial/build logs, incomplete matrices | Delete | Deleted after explicit user confirmation: no experiment was accepted and future harnesses must own results under `test/<name>/results/`. | Current producers moved to `test/ext4-journal-compare/`; no historical raw-data replacement is claimed. | `ext4_performance.md` records the retired location and current output path. | 49,357,299 B (47.07 MiB) |
| `ktest-logs/` (14 files) | Kernel-test serial/full diagnostic logs | Archive | Raw logs are ordinarily transient, but the current rTIO diagnosis directs readers to this collection. | `rNVPC-rTIO/rtio-flush-merge-and-thread-merge.md` is the retained diagnostic narrative. | `rNVPC-rTIO/rtio-flush-merge-and-thread-merge.md:141` references `ktest-logs/`. | 1,916,274 B (1.83 MiB) |
| `test/aster-ext4-baseline-test/plots/` excluding Python cache descendants (64 files) | Historical CSVs, plots, and final/intermediate figures | Keep | Current parser, patch, verifier, and plotting scripts address these inputs directly; no accepted evidence package replaces them. | None; inputs remain in place for the current harness. | `plot_baseline.py`, `plot_motivation.py`, `plot_writeback_matrix.py`, patch/verification scripts, and plot tests. | 6,014,062 B (5.74 MiB) |
| `test/aster-ext4-baseline-test/*.log` (5 files) | A/B diagnostic logs | Archive | Diagnostic records with uncertain future value; retain until the associated active harness work is stabilized. | No formal replacement. | Current baseline harness directory and `RESULTS.md` retain the diagnostic context. | 59,551 B (58.16 KiB) |
| `test/filebench-test/results/` excluding C1 caches (63 files) | Macro-run summaries, plots, and raw logs | Archive | No point is accepted as formal evidence; retaining avoids deleting data adjacent to current user-owned macro harness edits. | `EXPERIMENTS.md` zero-accepted authority. | Current `run-filebench.sh` result-path convention and `test/README.md` output policy. | 2,090,193 B (1.99 MiB) |
| `test/sqlite-test/results/` (33 files) | Macro-run summaries, plots, partial output, and raw logs | Archive | Includes explicitly partial/incomplete output, but the current uncommitted SQLite harness work makes deletion unsafe. | `EXPERIMENTS.md` zero-accepted authority. | Current `run-sqlite.sh` result-path convention and `test/README.md` output policy. | 823,818 B (804.51 KiB) |
| `test/macro-tests/results/` (5 files) | Macro smoke-run logs and manifest | Archive | Smoke output is not formal evidence; retain it until macro orchestration is finalized. | `test/macro-tests/run-macro-tests.sh` is the retained producer. | `test/macro-tests/run-macro-tests.sh` owns this output area. | 5,630 B (5.50 KiB) |
| `test/recovery-test/{summary.csv,crash-recovery-performance.pdf,crash-recovery-performance.png}` (3 files) | Generated recovery plot and data | Keep | These untracked generated artifacts sit beside untracked recovery source and test files; preserve the whole active harness set. | None; active working-tree artifacts remain in place. | `test/recovery-test/plot_recovery.py` and `test/recovery-test/test_plot_recovery.py`. | 185,447 B (181.10 KiB) |
| `test/aster-ext4-baseline-test/.build-stamp` (1 file) | Harness build-reuse state | Keep | The file is a cache, but deleting it changes the active harness's reuse behavior and it has retained script references. | Current build-reuse check in the harness. | `run-baseline.sh:342`, `rerun_bad_points.sh:48`, `run_rnvpc_bw_sweep.sh:77`, and `archive/debugging/2026-09-24-session-handoff.md:45`. | 40 B |
| C1, exact files below (25 files) | Reproducible Python bytecode caches | Delete | Regenerated by Python; every exact path and basename passed retained-reference checks. | Python source and tests regenerate the bytecode. | No retained references found; validation details below. | 160,832 B (157.06 KiB) |

## Exact deletion set C1

This fenced list is the complete expanded deletion boundary. It is intentionally
not a glob and does not authorize deletion of any containing directory.

```text
./__pycache__/parse_summary.cpython-38.pyc
./test/__pycache__/test_macro_profiles.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/parse_baseline.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/parse_mixed.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/plot_baseline.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/plot_motivation.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/plot_writeback_matrix.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/test_parse_remount_recovery.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/test_plot_baseline.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/test_plot_motivation.cpython-38.pyc
./test/aster-ext4-baseline-test/__pycache__/test_plot_writeback_matrix.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/breakdown-plot/__pycache__/plot_breakdown.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/evaluation-plot copy/__pycache__/plot_writeback_evaluation.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/evaluation-plot/__pycache__/plot_writeback_evaluation.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_breakdown.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_figure4_writeback_gap.cpython-38.pyc
./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_flush_scaling.cpython-38.pyc
./test/filebench-test/__pycache__/parse-filebench.cpython-38.pyc
./test/filebench-test/results/evaluation-plot/__pycache__/plot_filebench.cpython-38.pyc
./test/filebench-test/results/evaluation-plot/__pycache__/plot_sqlite.cpython-38.pyc
./test/recovery-test/__pycache__/plot_recovery.cpython-38.pyc
./test/recovery-test/__pycache__/test_plot_recovery.cpython-38.pyc
./test/sqlite-test/__pycache__/parse-sqlite.cpython-38.pyc
./tools/__pycache__/document_audit.cpython-38.pyc
./tools/__pycache__/test_document_audit.cpython-38.pyc
```

## Retained-reference validation for C1

For every C1 path, the following checks were run both with its exact path and
its basename:

```text
git grep -n -- '<path-or-basename>' -- '*.md' '*.py' '*.sh'
rg -n -F -- '<path-or-basename>' README.md STATUS.md EXPERIMENTS.md LESSONS.md evidence archive
```

All 25 per-path checks returned no retained reference. The nearby
`.build-stamp` was intentionally removed from C1 after the same validation
found live harness and archive references; it is classified `Keep` above.
No C1 entry is referenced by an authoritative/current document or script, so
there is no committed replacement or link to validate for this cache-only set.

## Totals and approval boundary

| Measure | Total |
| --- | ---: |
| Candidate groups | 10 |
| Candidate files | 1,085 |
| Delete files (C1 only) | 25 |
| Keep/Archive files | 1,060 |
| Estimated reclaimed bytes | 160,832 B |
| Estimated reclaimed human size | 157.06 KiB |

At approval time, no deletion, move, truncation, or artifact modification had
occurred. Approval applied only to the 25 exact C1 paths above.

## Post-cleanup execution record

The approved C1 deletion completed on 2026-09-27. All 25 literal paths were
present immediately before deletion and were verified absent afterward. No
empty parent directories were removed.

| Measure | Pre-cleanup | Post-cleanup | Delta |
| --- | ---: | ---: | ---: |
| Repository regular files excluding `.git` and `.worktrees` | 1,287 | 1,262 | -25 |
| Disk use excluding `.git` and `.worktrees` | 63M | 63M | 0M at `du -sh` precision |
| `results`, `test`, and `ktest-logs` regular files | 1,152 | 1,130 | -22 |
| Preserved C1 inventory logical bytes | 160,832 B | 0 B | -160,832 B (157.06 KiB) |

The preserved pre-deletion inventory in the appendix lists all 25 exact C1
paths and their sizes, summing to the manifest estimate of 160,832 B. Its
source also contained the intentionally retained 40 B `.build-stamp`, which is
excluded from the C1 total. The previously reported 159,612 B execution-time
aggregate is withdrawn: its per-path `stat` output was not retained, so the
exact logical bytes at the literal deletion moment are not independently
reproducible. `EXPERIMENTS.md` requires no update because its accepted-evidence
policy does not describe these disposable caches. The rounded repository size
remains 63M before and after cleanup.
The required documentation audit returned one pre-existing, out-of-scope issue:
`test/aster-ext4-baseline-test/archive/20260922-fsync-decomp/README.md` lacks
an `Archived` blockquote in its first 12 lines. It was deliberately not changed.

## Root diagnostic-log cleanup

On 2026-09-27, a separate explicit user approval authorized deletion of the
root-level `i5-*.log` and `smoke-*.log` files. The exact pre-deletion inventory
contained 31 untracked files totaling 67,384 B. Repository-wide literal-name
searches found no active document or script references. All matching root files
were deleted and a post-deletion `find` verified that no matches remained.


## Reproducibility appendix

All commands in this appendix were run at the post-`b28788d`, pre-deletion
snapshot stated above. Each aggregate table row has exactly one selection rule;
the `awk` program prints its reproducible regular-file count and byte total.
The C1 list above remains the sole deletion authorization and is not derived
from these aggregate selectors.

```text
[M1] find . -path './.git' -prune -o -path './.worktrees' -prune -o -type f -print | wc -l
1287
[M2] du -sh --exclude=.git --exclude=.worktrees .
63M	.
[M3] find results test ktest-logs -type f 2>/dev/null | wc -l
1152

[R]  find results -type f -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=872 bytes=49357299
[K]  find ktest-logs -type f -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=14 bytes=1916274
[BP] find test/aster-ext4-baseline-test/plots -type f ! -path '*/__pycache__/*' -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=64 bytes=6014062
[BL] find test/aster-ext4-baseline-test -maxdepth 1 -type f -name '*.log' -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=5 bytes=59551
[FB] find test/filebench-test/results -type f ! -path '*/__pycache__/*' -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=63 bytes=2090193
[SQ] find test/sqlite-test/results -type f -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=33 bytes=823818
[MS] find test/results -type f -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=5 bytes=5630
[RC] find test/recovery-test -maxdepth 1 -type f \( -name 'summary.csv' -o -name 'crash-recovery-performance.pdf' -o -name 'crash-recovery-performance.png' \) -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=3 bytes=185447
[ST] find test/aster-ext4-baseline-test -maxdepth 1 -type f -name '.build-stamp' -printf '%s\n' | awk '{n++; bytes += $1} END {printf "files=%d bytes=%d\n", n, bytes}'
files=1 bytes=40
[C1] exact 25-path list above, summed with stat -c '%s'
files=25 bytes=160832
```

## Preserved C1 Per-Path Size Evidence

The following pre-deletion inventory was copied from
`/tmp/task5-delete-caches.tsv`, captured at `2026-09-27 13:03:59 +0800`.
That file's SHA-256 was
`1b9c900b701141a2eb388de66e9d49f2c7de2ddf99ce14e212838f9f1c6ed4a1`.
It contained these 25 C1 rows plus the separately retained `.build-stamp` row;
only the C1 rows are reproduced and summed here.

| Path | Pre-delete bytes |
| --- | ---: |
| `./__pycache__/parse_summary.cpython-38.pyc` | 5,115 |
| `./test/__pycache__/test_macro_profiles.cpython-38.pyc` | 3,448 |
| `./test/aster-ext4-baseline-test/__pycache__/parse_baseline.cpython-38.pyc` | 7,081 |
| `./test/aster-ext4-baseline-test/__pycache__/parse_mixed.cpython-38.pyc` | 3,702 |
| `./test/aster-ext4-baseline-test/__pycache__/plot_baseline.cpython-38.pyc` | 8,204 |
| `./test/aster-ext4-baseline-test/__pycache__/plot_motivation.cpython-38.pyc` | 8,549 |
| `./test/aster-ext4-baseline-test/__pycache__/plot_writeback_matrix.cpython-38.pyc` | 8,532 |
| `./test/aster-ext4-baseline-test/__pycache__/test_parse_remount_recovery.cpython-38.pyc` | 4,345 |
| `./test/aster-ext4-baseline-test/__pycache__/test_plot_baseline.cpython-38.pyc` | 7,301 |
| `./test/aster-ext4-baseline-test/__pycache__/test_plot_motivation.cpython-38.pyc` | 8,738 |
| `./test/aster-ext4-baseline-test/__pycache__/test_plot_writeback_matrix.cpython-38.pyc` | 8,856 |
| `./test/aster-ext4-baseline-test/plots/breakdown-plot/__pycache__/plot_breakdown.cpython-38.pyc` | 5,555 |
| `./test/aster-ext4-baseline-test/plots/evaluation-plot copy/__pycache__/plot_writeback_evaluation.cpython-38.pyc` | 1,220 |
| `./test/aster-ext4-baseline-test/plots/evaluation-plot/__pycache__/plot_writeback_evaluation.cpython-38.pyc` | 1,424 |
| `./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_breakdown.cpython-38.pyc` | 11,114 |
| `./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_figure4_writeback_gap.cpython-38.pyc` | 4,713 |
| `./test/aster-ext4-baseline-test/plots/flush-plot/__pycache__/plot_flush_scaling.cpython-38.pyc` | 8,337 |
| `./test/filebench-test/__pycache__/parse-filebench.cpython-38.pyc` | 6,439 |
| `./test/filebench-test/results/evaluation-plot/__pycache__/plot_filebench.cpython-38.pyc` | 7,098 |
| `./test/filebench-test/results/evaluation-plot/__pycache__/plot_sqlite.cpython-38.pyc` | 6,523 |
| `./test/recovery-test/__pycache__/plot_recovery.cpython-38.pyc` | 8,075 |
| `./test/recovery-test/__pycache__/test_plot_recovery.cpython-38.pyc` | 2,634 |
| `./test/sqlite-test/__pycache__/parse-sqlite.cpython-38.pyc` | 5,307 |
| `./tools/__pycache__/document_audit.cpython-38.pyc` | 8,318 |
| `./tools/__pycache__/test_document_audit.cpython-38.pyc` | 10,204 |
| **C1 total** | **160,832** |
