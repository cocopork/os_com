# os_com EXT4 / AchillesFS Research Implementation

> 2026-10-18：本目录已迁入 `os_com_rnvpc_rebuild/os_com_implementation/`，
> 作为代码仓库的普通目录统一版本管理，不再是独立 Git 仓库。
> 崩溃前的设计、状态与历史验收记录按原样保留；恢复后的当前实现和实验见
> [恢复报告目录](../docs/recovery/)及[本次迁移记录](../docs/recovery/REPOSITORY-CONSOLIDATION-2026-10-18.md)。

## What This Directory Contains

This directory is the documentation, experiment-harness, and design companion
to the `os_com` Asterinas EXT4 implementation. It records the project contract
and links to implementation evidence; it does not duplicate mutable progress or
turn historical notes into current claims.

The four root authorities divide responsibility deliberately:

- [`STATUS.md`](STATUS.md) preserves the pre-crash implementation and validation status; current rebuild evidence is in `../docs/recovery/`.
- [`EXPERIMENTS.md`](EXPERIMENTS.md) records the acceptance boundary for performance evidence.
- [`LESSONS.md`](LESSONS.md) condenses verified engineering lessons.
- This document provides navigation, repository relationships, and safety constraints.

## Current Entry Points

| Need | Path from this README |
| --- | --- |
| Current rebuild validation | [Recovery reports](../docs/recovery/) |
| Historical implementation progress and unverified work | [`STATUS.md`](STATUS.md) |
| Accepted performance conclusion and CSV | [`EXPERIMENTS.md`](EXPERIMENTS.md) (1 link; no experiment is currently accepted, so no CSV is presented here) |
| Experiment acceptance, diagnostic material, and pending matrix | [`EXPERIMENTS.md`](EXPERIMENTS.md) (1 link) |
| Verified correctness, performance, and methodology rules | [`LESSONS.md`](LESSONS.md) |
| AchillesFS authoritative data model, persistence, recovery, and invariants | [`rNVPC-rTIO/design.md`](rNVPC-rTIO/design.md) (1 link) |
| Approved rNVPC persistent-WAL foreground-checkpoint specification | [`rNVPC-rTIO/rnvpc-foreground-checkpoint-design.md`](rNVPC-rTIO/rnvpc-foreground-checkpoint-design.md) |
| AchillesFS implementation work table | [`rNVPC-rTIO/implementation.md`](rNVPC-rTIO/implementation.md) |
| EXT4 refactor work table | [`os_com_refactor_plan/README.md`](os_com_refactor_plan/README.md) |
| One-command non-destructive test reproduction | [`test/aster-ext4-baseline-test/README.md`](test/aster-ext4-baseline-test/README.md) (1 link; `bash run-baseline.sh --dry-run`) |
| Generic filesystem development skill | [`.codex/skills/filesystem-development/SKILL.md`](.codex/skills/filesystem-development/SKILL.md) (1 link) |
| Project-specific AchillesFS skill | [`.codex/skills/asterinas-ext4-achillesfs/SKILL.md`](.codex/skills/asterinas-ext4-achillesfs/SKILL.md) (1 link) |

## Repository Relationships

The parent directory (`..`) contains the kernel and the single Git repository.
This directory contains design documents and host-side experiment harnesses.
Commit related code and documentation together on `recovery/rnvpc-rebuild`,
using the code repository's existing `origin` remote. Do not initialize another
Git repository here or add this directory as a submodule.

Host entry scripts resolve paths through `test/host-paths.sh`. Their historical
container names, devices and workload profiles remain historical defaults;
path migration alone does not validate them against the current rebuild.
Use the current experiment reports in `../docs/recovery/` for verified settings.
Historical paths and commit identifiers in archived plans are provenance, not
instructions to recreate the retired repositories.

## Experiment Safety

Real-device formatting and QEMU/KVM performance runs are destructive or
resource-contended operations. Use one owner at a time for a target device, do
not overlap performance runs, confirm the target has no mount point, and retain
the command, device, workload, code revision, instrumentation state, and guest
completion marker with every candidate result.

SATA and NVMe comparisons use the same five harness profiles. They are distinct
experimental objects, not proof of an implemented AchillesFS performance result:

| Object | Harness profile | Journal | Writeback behavior |
| --- | --- | --- | --- |
| ext2-orderless | `ext2like_sync` | disabled | wait for each BIO |
| ext4-orderless | `ext4_sync` | enabled | wait for each BIO |
| ext4-rNVPC surrogate | `ext2like_nomerge` | disabled | asynchronous, no merge |
| ext4-rNVPCe surrogate | `ext4` | enabled | asynchronous, merged |
| ext4-nomerge isolation control | `ext4_nomerge` | enabled | asynchronous, no merge |


All five profiles use the shared EXT4 format and common writeback settings
defined by the harness. `wb_sync_wait=on` waits after each submitted BIO;
`wb_merge=off` prevents contiguous dirty pages from being merged and yields one
4 KiB BIO per page. The fifth profile isolates the merge effect from the journal
effect, which the third and fourth profiles would otherwise change together.

The target comparison ordering is `ext2-orderless <= ext4-orderless <=
ext4-rNVPC <= ext4-rNVPCe`; it is an experiment goal, not a guaranteed result.
The first two profiles have no independent ordering control, so a measured
ordering must not be forced by a distortion-prone setting. A smoke run only
establishes whether a difference is measurable; formal results require repeated
observations and a stated aggregation rule.

The harness profile appears in `summary.csv`; exact mounted options are recorded
in the serial-log `PARAM mount_opts` line. This preserves the configuration
provenance without pretending that the profile name is the complete mount
configuration.
The labels `rNVPC` and `rNVPCe` in this matrix are surrogates and must not be
reported as measurements of those mechanisms themselves. Formal throughput
claims use `busy_bw_mib_s`; drain metrics are diagnostic-only checks for tail
completion and writeback behavior. Full configuration constraints and evidence
rules are in [`EXPERIMENTS.md`](EXPERIMENTS.md).

## Archive Policy

Historical handoffs, rejected approaches with lasting diagnostic value, and
superseded design material live below `archive/`. Archive documents begin with
an `Archived:` banner and are not current authorities. The historical handoff
for the five-profile transition is
[`archive/debugging/2026-09-24-session-handoff.md`](archive/debugging/2026-09-24-session-handoff.md).
