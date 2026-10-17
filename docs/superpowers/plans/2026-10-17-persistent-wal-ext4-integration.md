# Persistent WAL and EXT4 integration implementation plan

**Goal:** Restore rNVPC persistent WAL/checkpoint, audit and complete rNVPCe soft updates, and connect both modes to EXT4 file operations.
**Architecture:** Separate rNVPC WAL-backed PageCache tree from WAL-free rNVPCe extent trees. Reuse shared bounded NVPool, identity/lease/persistence primitives. Restore source evidence before writing missing code.
**Tech Stack:** Rust nightly-2026-04-03, Asterinas OSDK 0.18.0, isolated recovery/rnvpc-rebuild worktree.
**Spec:** /home/zn/zn/Rust_EXT4/os_com_implementation/rNVPC-rTIO/design.md and rnvpc-foreground-checkpoint-design.md; latest STATUS.md supersedes older implementation prose.

## Constraints
- 1024 WAL records/inode; write 1025 synchronously replays all records by LSN without coalescing. Persist tree and anchor before retiring head.
- SSD completion never retires WAL; wal-pinned pages cannot migrate.
- rNVPCe remains WAL-free: direct NV copies, data before references, durable allocation identities, generation-aware retry/reclaim.
- Mode selection, read/write, sync/unmount, SSD reservations and migration must form real call chains; no stubs or disabled production features.
- Kernel safe Rust; no raw devices; use disposable test images only. Preserve original evidence and checkpoint.
- Existing tests/build red because 13 missing EXT4 symbols (17 diagnostics), plus disconnected VFS calls.

## Tasks
- [x] Recover persistent WAL/layout/store/B-link tree and matching cow/session APIs; build focused tests for 1024/1025, repeated keys, replay, faulted publication, SSD retirement independence.
- [x] Recover EXT4 supporting fs/inode/VFS/mode code from candidates/sessions; wire two distinct routes, migration workers and sync exports, preserving baseline normal mode.
- [x] Audit rNVPCe ordering, repair, reservation and reverse-index/reclaim contracts; fix evidence-supported gaps, explicitly track remaining proof gaps.
- [x] Integrate contracts in rnvpc module entry and shared frame/recovery, resolve compilation failures without fake implementations.
- [x] Build normal and ktest configurations; run focused kernel tests and disposable-image smoke where builds permit.
- [x] Independent review, necessary fixes and final provenance/validation report.

## Review focus
- Durable commit failure, torn selector, LSN ordering and duplicate-key checkpoint.
- Slot reuse and stale migration completion must not reclaim new objects.
- User-copy faults must publish only durable successful prefixes.
- Recovery must not drop rNVPC anchors/tree/WAL as rNVPCe orphans.
- IRQ callback cannot persist or mutate extent metadata; no blocking while spinlocked.

## Progress
User explicitly authorized implementation against existing design; continuing without a new design approval round. Baseline file/source manifest and prior failed build retained in docs/recovery and host build artifacts.

Final implementation and source review complete; ktest-04: 56 passed, 0 failed. Final normal kernel make kernel exited 0 (build-final.log). The SSD allocation/target crash ownership gap is explicitly unresolved, with bounded runtime mitigation; this plan completion is not a claim of complete crash-safe recovery. See docs/recovery/WAL-EXT4-INTEGRATION.md.
