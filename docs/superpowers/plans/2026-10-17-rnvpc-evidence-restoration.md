# rNVPC evidence restoration implementation plan

**Goal:** Rebuild the 11 missing rNVPC files from candidate/reference/session evidence, preserving provenance and testing compatibility rather than hiding missing functionality.
**Architecture:** Keep exact restored checkpoint modules as contract consumers. Search available historical blobs and session views first. Adapt complete old reference files only with evidence for changed semantics. New implementation is a last resort and must be explicitly labeled. Work only in os_com_rnvpc_rebuild, never original evidence or the checkpoint tree.
**Tech Stack:** Rust nightly-2026-04-03, OSDK 0.18.0, asterinas development container, Python evidence tooling.
**Spec:** /home/zn/zn/Rust_EXT4/os_com_implementation/rNVPC-rTIO/design.md; STATUS.md; recovered checkpoint consumers.

## Constraints
- Do not substitute empty functions or disable rNVPC to make compilation pass.
- Existing exact modules target checkpoint 3b48118ae; later session changes are separately marked.
- Durable writes, generation identity, reservation accounting, safe recovery and lock contracts cannot be guessed from names.
- File recovery does not prove whole-kernel functional recovery; report unvalidated pieces explicitly.
- No raw device access, push or destructive tests.

## Tasks
1. Audit Git history and session coverage; record available bases per missing module (historical blob audit now complete: no available old blobs among traversable history).
2. Reconstruct backend/config/dram from old nv_pool files plus contemporary API and session evidence; retain tests for bounds, modes, direct copy and persistence gates.
3. Reconstruct descriptor/index/metadata from candidate and positioned session definitions; validate layout, identity, reservation and mutation record contracts against persist/frame/extent_mutation/write_transaction.
4. Reconstruct cow/extend_cow/recover from historical implementations and session evidence; distinguish obsolete WAL retirement and old rNVPCe CoW semantics. Do not label incomplete algorithms complete.
5. Reconstruct module entry and focused tests using recovered consumers and session traces; compile production and ktest configurations, inspect errors by subsystem.
6. Review changes and provenance; run meaningful focused tests where compilation permits. Leave a concrete status for any remaining coupled EXT4/PageCache recovery gaps.

## Review focus
- Never accept stale slot generations as valid references.
- Never retire WAL on SSD completion in the final foreground-checkpoint object.
- Never reclaim before durable removal/publication.
- Never revive a cleared pending mark or silently accept corrupt recovery roots.
- No ordinary lock held across user page faults/asynchronous I/O.

## Decisions and baseline
- Baseline make kernel already fails with E0583 and four mixed-version EXT4 errors; authorized recovery work proceeds from that recorded red baseline.
- Original tree remains at 49e7829d6; this branch is recovery/rnvpc-rebuild.
- Complete old reference files exist for six missing paths. Partial evidence must remain distinguishable from verified originals.
- Delegation uses subagent-driven-development for separate file ownership; parent handles integration and testing. Do not commit unreviewed reconstructions as exact recovery.
