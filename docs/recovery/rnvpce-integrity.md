# rNVPCe integrity integration

This change restores the later WAL-free leaf-completion implementation against the approved `rNVPC-rTIO/design.md` and latest `STATUS.md`. It is an integration candidate, not a claim that the entire historical working tree or crash matrix has been recovered.

## Source evidence

Evidence root: `/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016` (read only).

- `extent_mutation.rs`: exact bytes from `post-checkpoint-candidates/kernel/src/fs/rnvpc/extent_mutation.rs`, Git blob `d7897cbb86a7d8577de294bf6cc61d6a0a2ec690`, SHA-256 `fb3ed9e3af1fcca2acafdde8099f831c2ee1618a066d29c8bb32887603b1832e`, observed fingerprint dated 2026-10-13 07:03:29 UTC.
- `write_transaction.rs`: exact bytes from the corresponding candidate, Git blob `41d5ab44c9f722c1f545d2209e806da652e45add`, SHA-256 `56fc441845c63d2e7a47525576e57496b9cca02ef1fe8a22b4f8a6a4ae13ad66`, observed fingerprint dated 2026-10-13 04:44:09 UTC.
- `post-checkpoint-candidates.json` records both anchors as complete reconstructed bodies matching observed diff fingerprints, with no subsequent applied patches. Neither is claimed to be the exact checkpoint or complete final working tree.
- `metadata.rs` was audited and left unchanged: typed records, 16-byte `(slot, allocation_seq)` references, 8-byte alignment, checked validation, and existing persistent field layouts remain intact. No new rNVPCe log or persistent schema was introduced.

The matching `frame.rs`/object-table restoration supplies generation-checked metadata mutation guards, guarded in-place updates and the pending-mark lookup directory. The EXT4 integration supplies matching diagnostic fields. These are explicit cross-file dependencies.

## Audited contracts

1. **Direct copy before references:** `commit_range_write_with_mutation` copies directly into an existing mutation lease or zero-initialized reserved staging page. Existing-page copying persists the copied range and fences; a fresh page obtains a committed descriptor before an extent references it. Copy failure after a prior successful page terminates the returned prefix. A fresh page with no copied bytes is not published.
2. **Reservation before reachability:** count-only SSD reservation uses actual newly mapped pages after copying, before replacement extent publication. Fixed-inode size is derived from `file_offset + copied`, and block accounting from newly mapped pages. Concrete SSD allocation remains migration-time work owned by EXT4.
3. **Fixed roots:** foreground publishes through `update_fixed_inode`, retaining the inode reference. Duplicate fixed roots must be rejected by recovery; that correction is owned by the parent integration, outside these two restored files.
4. **Repair:** foreground retains metadata node guards through reverse installation. Repair images precede the pending mark and in-place rewrite. Persistent mark invalidation precedes repair-frame reuse. Completion acquires one leaf guard, rejects a pending mark touching that leaf, validates leaf owner and each reverse/forward edge, then publishes one combined leaf image. A stale grouping is re-resolved with a bounded retry.
5. **Independent ledger:** reserve and consume now serialize on the reservation record's own guard, avoiding lost updates when completion runs without inode publication. Recovery-only reconciliation derives outstanding capacity from the reachable graph.
6. **Migration credits:** successful SSD publication precedes the one-way descriptor credit phase. Each newly advanced phase returns a true per-page outcome to the EXT4 runtime-credit owner. Failed persistent ledger refresh is logged and does not suppress that outcome; recovery reconciles the conservative ledger. Retrying the same phase returns false.
7. **WAL isolation:** neither restored production file imports or appends WAL records. Separate rNVPC anchor preservation during shared recovery/reclamation is owned by the parent/WAL integration.

An audit observation was sent to the frame/EXT4 owners: `retry_migration` clears a descriptor-local target, whereas ordinary I/O failure must retain it. Its callers and ownership-release behavior require cross-file review; this report does not assume that every retry call is an I/O-failure call.

## Tests and limits

New module: `kernel/src/fs/rnvpc/rnvpce_integrity_tests.rs`.

- Reservation reserve/consume concurrency and pending-mark reader/update/reclaim exclusion are recovered complete function bodies from positioned view `tests.rs/64469a7bc.rs.partial`; surrounding incomplete regions were not compiled.
- Credit-release/failed-ledger idempotence is a complete recovered body from raw candidate `os_com_candidates_20261015T074727Z/candidates/02_project_rust/6e/6ed2ca1cab448dfeb6861e9b161650684b23a4fbd5031ba4720293bdeb2e42eb__00071132.txt`.
- Batched leaf migration is recovered from `review-bundle/session-changes/kernel/src/fs/rnvpc/tests.rs/0061.patch`. Adaptations: the second page's page-local offset is zero (confirmed by later positioned evidence), sectors-per-block is one page divided by 512, completion runs while an otherwise untouched foreground inode publication owner is held (later `64469a7bc` evidence), and reclamation uses the later combined `reclaim_after_ssd_publish` API. Retry must consume no second reservation credit; a second explicit free must return false.
- A newly authored short-reader integration regression requests three pages but supplies one page plus 37 bytes. It checks the returned prefix, exactly two credits, stable inode slot, absence of the third mapping, recovered data/zero tail, rebuilt reverse entries, and idempotent second restart. This tests a bounded reader, **not** an actual CPU/user-page fault.

All five tests are real kernel tests; the parent adds the module to `mod.rs`. Rustfmt successfully parsed/formatted the new module using nightly-2026-04-03 in the project container. Full typecheck and VM execution are parent integration gates and are not asserted here. Full split/root-growth fault matrices, actual user-copy fault injection, over-capacity filesystem lifecycle and hardware persistence validation remain outside this focused proof.

## Shared frame adapter for persistent rNVPC

Additional integration ownership: new `frame/pcache.rs` only. This is an explicitly adapted module, not an exact recovered file. Historical evidence was read before implementation:

- `/home/zn/asterinas-build-env/wal-ext4-integration/frame-sessions/11508.txt`: generation/owner-checked persistent data pin admission.
- Positioned `frame.rs/c0050c84d.rs.partial`: complete metadata update lease, range persistence, fence and drop behavior.
- Positioned `frame.rs/a1fd547b2.rs.partial` and `4f6ca6180.rs.partial`: typed descriptor commit callback boundary, abort release, and atomic generation-checked metadata clear/free.
- Positioned `frame.rs/1ead1bd06.rs.partial` and session `1868.txt`: typed object enumeration, catalog peek, and removal only after successful foreground recovery; the latter also supplies the exact data reclamation body.
- `/tmp/evidence-11509.txt`: complete historical `PersistentMetadataStore` consumers and persistence plan protocol.

Adaptations to the current shared pool: replace global record/capacity locking with per-slot records and atomic pin telemetry; insert/remove the current metadata index; perform clear/free finalization in control → allocation → slot lock order; retain a newer data-index handle when clearing an older generation; reject rNVPCe reverse-referenced or legacy-referenced data in the rNVPC exact reclaim API; close metadata reader admission and drain existing readers through the current exclusive-access primitive. Persistent data pins additionally expose reads, keeping identity validation and byte access in one lease. All persistence remains outside spinlocked sections.

Typed commits disarm the caller's allocation cleanup immediately before descriptor persistence, retaining the allocated frame on an ambiguous persistence error. Preflight errors release only their pin and leave the caller's allocation cleanup armed. The descriptor retains the current durable allocation sequence. Metadata clear errors retain committed state and capacity for retry. `committed_typed_metadata_for` returns logical indices to match the recovered WAL consumers; global enumeration retains full `PageKey`s for recovery. Catalog `take` is non-fallible and does not trigger a scan, matching the later peek/finish protocol.

Rustfmt parsed the adapter under nightly-2026-04-03; whole-crate typecheck, WAL fault tests and VM validation remain integration-owned. The adapter does not introduce a WAL path into rNVPCe.
