# rNVPCe Nojournal Implementation Plan

> Execute inline with executing-plans and test-driven-development. No commit or push is requested.

**Goal:** Make rNVPCe mounts use the existing nojournal persistence path without silently ignoring outstanding journal recovery.

**Architecture:** Validate clean journal state before starting any filesystem worker. Retain the disk journal feature for later native mounts, but instantiate no runtime journal in rNVPCe. Preserve migration metadata synchronization and SSD flushes.

**Tech Stack:** Rust, Asterinas EXT4, kernel ktests, Docker/KVM, fio.

**Spec:** README.md in this directory; full metadata crash-consistency coverage remains an explicit separate acceptance item within this requirement.

## Global Constraints

- No NVPage batch-transaction changes; preserve native EXT4 and rNVPC WAL behavior.
- Dirty logs fail before mutation; no automatic feature-bit removal or journal erasure.
- Existing unsupported operations stay unsupported; no new complete-filesystem crash-consistency claim.
- Work in the existing recovery branch, preserving all pre-existing uncommitted changes.

## Review Focus

- Dirty journal start with a clear filesystem RECOVER bit must still be rejected.
- Orphan or RECOVER flags on a journal-less image must not bypass the guard.
- A failed mount must not write metadata from an Ext4 Drop error path.
- Device flush errors must not lead to NVPage reuse.
- Returning to native mode with retained NVM authority must remain rejected by existing mode validation.

## Task 1: Mount policy and mode isolation

Files: `kernel/src/fs/fs_impls/ext4/fs.rs`, `nv_tests.rs`, `test_utils.rs`, `inode/mod.rs`, `inode/extent_manager/mod.rs`.

- [x] Add scenario in existing `ext4_research_routes` that mounts a clean journal fixture with rNVPCe, asserts no runtime journal/handle/RECOVER write, migrates data and verifies exported SSD bytes.
- [x] Add recovery guard scenarios by modifying fixture disk superblock and journal start after a clean unmount; snapshot disk bytes before failed open and assert byte-for-byte unchanged.
- [x] Run the route ktest and retain RED evidence.
- [x] Validate journal geometry once on mount, refuse recovery-needed rNVPCe before creating workers or dirtying metadata, and execute normal journal lifecycle only in the other modes.
- [x] Run route and native journal suites, retain GREEN evidence.

## Task 2: Build and workload evidence

Files: existing fio harness and a new `docs/experiments/reports` validation report.

- [x] Build release and run full kernel suite in the documented SMP=1 configuration; separately retain any SMP findings without calling failures passes.
- [x] Use the authorized NVMe partition, 2GiB total and64MiB data limit, to verify 4KiB random and128KiB sequential writes, successful sync/unmount and e2fsck.
- [x] Record effective journal mode, original disk feature, fio bytes/errors and device flush evidence. Keep throughput distinct from crash-consistency validation.
- [x] Review the final diff for mount failure side effects and mode isolation; record unresolved metadata recovery coverage explicitly.

## Execution ledger

2026-10-18: user changed order to nojournal first; requirements split as009 and010. No repeat approval requested because implementation is explicitly authorized. No plan to change128-page migration window or descriptor protocol.

2026-10-18 implementation ruling: the first2GiB nojournal sequential point completed without error but final sync took197.26s (fio16.687s). Native export wrote one inode per page; test reproduced9 writes for9 pages. Reuse nojournal node after-image batching in bounded64-block export chunks, writing inode after node writeback. This belongs to nojournal SSD export compatibility, not NVPage target/credit/reclaim batching. New sparse external-leaf and inode-table failure/retry scenarios cover the change. Keep the original point as pre-export-fix evidence; do not mix its ISO/hash with final results.

2026-10-18 verification: original mount-policy RED and export-write-count RED preserved. Final full suite302 passed/0failed (SMP1). One intermediate suite301/1 exposed an incorrect test assumption about fresh-node allocation rollback; corrected to assert exactly one leaf on successful retry, and separately tested retained after-images on an existing leaf. Runtime test behavior, not production code, changed for that assertion. Final release ISO built successfully; real-disk matrix in progress.

Final validation:302/0 SMP1; final release built;4main2GiB cases and1additional2GiB no-disk-journal case completed fio/sync/unmount/e2fsck. Full metadata power-loss consistency remains open in the requirement. No commit or push.
