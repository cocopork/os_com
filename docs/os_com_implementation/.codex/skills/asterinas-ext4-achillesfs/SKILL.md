---
name: asterinas-ext4-achillesfs
description: "Develop or validate Asterinas EXT4 and AchillesFS mechanisms using current project authorities and test constraints."
---

# Asterinas EXT4 AchillesFS

Use this skill for the project-specific implementation, validation, or diagnosis
of EXT4, JBD2, PageCache, extent, rNVPC, rNVPCe, rTIO, and their harnesses.

Read these authorities in order before making a current-state claim or proposing
validation:

1. [Generic filesystem guidance](../filesystem-development/SKILL.md)
2. [Current status](../../../STATUS.md)
3. [Experiment acceptance](../../../EXPERIMENTS.md)
4. [AchillesFS design](../../../rNVPC-rTIO/design.md) or
   [implementation map](../../../rNVPC-rTIO/implementation.md), as applicable

Use the live authorities rather than embedding completion state, dates, or
performance values in this skill. Read the appropriate concise reference for
stable project context:

- [Architecture](references/architecture.md): ownership and persistent versus
  volatile state.
- [Environment](references/environment.md): build, virtualization, device, and
  harness isolation.
- [Known issues](references/known-issues.md): evidence-backed failure patterns
  and validation boundaries.

Record the source revision and dirty state before running validation. Preserve
unrelated working-tree changes and treat incomplete or missing completion
evidence as non-results.
