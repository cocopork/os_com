# Independent integration review

Read-only production-code review of the active recovery worktree against `design.md`, `rnvpc-foreground-checkpoint-design.md`, and `2026-10-17-persistent-wal-ext4-integration.md`. Reviewed while implementation/build continued; line numbers below identify the reviewed snapshot and can shift. No build or runtime tests were run by this reviewer. Compilation alone is not acceptance evidence.

## Final targeted follow-up after ktest03

The integrator reports ktest03 ran 51 tests: 50 passed and one failed on shared-pool payload decoding. This is actual test selection, unlike ktest01; the post-fix suite outcome remains pending here.

Source inspection confirms typed page-cache metadata is now excluded **before** soft-update decoding in `frame::extent_references_data`, both later reservation/allocation scans in recovery, and `write_transaction::outstanding_ssd_capacity`. The latter also filters filesystem identity before decoding. `Rnvpc::validate_fs_mode` was added to reject incompatible retained ownership; the mount path now calls it immediately after computing `rnvpc_fs_id`, before loading block groups or creating the filesystem object.

The new rNVPC write sequence explicitly captures/persists native allocation ownership before `context.update` can expose a WAL reference, reacquires inode inner and checks the concrete target again, then captures and syncs native size before counting the successful user prefix. Durability waits occur after dropping inode inner and the journal operation. User input is consumed once and the current partial-page image is rebuilt after waits. This necessarily adds native metadata/journal/device synchronization to the foreground per-page path; performance comparisons must report that overhead rather than attribute the entire write cost to WAL append/checkpoint alone.

Two narrow corrections were requested during this final review:

- Both added barriers initially used `sync_data_and_meta(DataOnly)` after `write_back_inode_desc_forced` had cleared dirty state, but the capture stamps only `sync_tid`, not `datasync_tid`. They must stamp `datasync_tid` after successful capture or use the Full barrier. Otherwise a journaled filesystem can wait on an older/no transaction despite the apparent barrier.
- Finding 5 now has a runtime migration-error latch after ambiguous target assignment and attempts a normal compensation free before target publication. If that compensation itself fails, it must latch too; otherwise repeated allocation/rollback failures remain unbounded. This does not close the allocation-to-descriptor crash window.

Both narrow corrections were subsequently inspected and verified in source. Each native rNVPC capture snapshots `inner.is_dirty()` before forced capture and stamps `datasync_tid` only after a successful real capture. This waits for the data-relevant transaction while avoiding a wait on a fresh empty operation that cannot commit. The rollback-failure branch now latches migration failure too. The integrator reports ktest04 has started with 56 selected tests; its result is not yet available to this reviewer. No additional broad scope was requested.

## Follow-up source review after build04

The integrator reports the whole-kernel build04 passed. ktest01 booted but selected zero tests because its filter was wrong; this is **not runtime test coverage**. This reviewer performed no compilation or execution.

| Finding | Follow-up disposition |
| --- | --- |
| 1, rNVPC busy lease | Source fix verified. `pin_committed_data_ref` returns EBUSY for an active lease. Both foreground methods sample the epoch before probing and release inode inner before waiting. Writes also release their journal op, retain the already copied user prefix, reload current bytes, and handle the second race at `context.update`. Concurrent migration/read/write tests remain needed. |
| 2, native graph mutations | Source fix verified. `ensure_native_mutation` rejects both non-Native modes; resize, fallocate, regular unlink and regular rename-overwrite invoke it. Native operations retain their path. |
| 3, export exclusion | Source fix verified. Export acquires inode inner then publication, restores authority under them, and updates native extents through the already held inner guard. No recursive inner acquisition remains in this path. |
| 4, completion ownership | Source fix verified. The queue is now owned by Ext4, not one submission round. Submission and completion are separate workers; flight credits are returned after process-context draining and lease drops. Ext4 Drop drains outstanding credits before destroying fields. Later preparation errors therefore cannot make an old callback the last owner of its completion queue. `Bio::submit` rejects enqueue without callback completion, consistent with submission-error credit rollback. Test enqueue errors, completion errors, and last-owner teardown with in-flight requests. |
| 5, SSD ownership window | **Unresolved.** Allocation is still persisted before persistent target assignment; see bounded mitigation below. |
| 6, physical-view and worker architecture | Source fix verified. BIO segments use `EvictingDataPage::dma_segment`, directly exposing the pinned backend page; no stack/data bounce remains. Two workers use an Ext4-owned bounded queue and 128-page in-flight admission. Contiguous physical targets are grouped into device-bounded BIOs; completion groups by inode before leaf publication. |
| 7, recovered inode export discovery | Source fix verified. Sync enumerates `pool.fixed_inode_ids(fs_id)` from committed fixed-root metadata rather than the live-access set; recovered-but-untouched roots are included and validated. |

### Bounded mitigation for finding 5 (initial proposal; implementation follow-up above)

Once a concrete block has been allocated, any allocation-durability or target-assignment failure must stop automatic reallocation for that unresolved operation. A filesystem migration-error latch plus a retained runtime diagnostic identity `(fs_id, inode, page slot, allocation sequence, SSD target)` can make later migration/sync report the error rather than repeatedly allocate and leak more blocks. This bounds repeated-failure leakage within one running instance. It leaves ordinary successful migration enabled.

A normal journaled compensation free is safe only if target publication has not started and ownership of the unreferenced allocation is proven. Once descriptor persistence was attempted, do not free the SSD block merely because the operation returned an error: its target may already be durable. A read of the working image is not proof of the durable image. Recovery-aware durable ownership requires a larger protocol; the runtime latch does **not** remove the crash window and is not crash-consistency proof.

The earlier findings below are retained as the review trail, not a claim that all seven remain open. Remaining acceptance blockers are finding 5's explicit durability limitation and missing runtime/fault/concurrency evidence; build success alone does not resolve them.

## Findings sent to integrator

1. **High: rNVPC foreground reads/writes reject pages during ordinary migration.** `frame/pcache.rs::pin_committed_data_ref` rejects `record.persisting` with EINVAL. `frame.rs::begin_data_page_eviction_inner` sets persisting through BIO/completion. `ext4/inode/mod.rs::read_rnvpc_at` and `write_rnvpc_at` read the page while holding native inode inner, so a migrating checkpointed page produces a user-visible error. Return a retryable busy status and re-resolve the mapping after a lease-epoch wait; release native inode inner before waiting because migration completion needs it. This is newly reachable through the integration.

2. **High: native-only graph mutations must be rejected in both NV modes.** At initial review `ext4/inode/mod.rs:2110` resize had no NV guard. Native truncate/fallocate/unlink/rename-overwrite can release SSD allocations or reuse inode identity while persistent WAL/tree or extent references remain. Subsequent migration can overwrite another inode's reused SSD block. Guards are necessary for rNVPC as well as rNVPCe until graph-aware mutation exists. Native methods are historical; their unsafe NV reachability is new. The EXT4 implementer is actively adding guards; recheck final source.

3. **High: export lacks required exclusion.** At review `ext4/inode/mod.rs:1177` export calls `write_transaction::visit_ssd_extents` with no inode-inner or publication exclusion, despite the explicit caller contract at `write_transaction.rs:1624`. It reacquires inner separately per exported entry. A concurrent write/split can change the selected graph between snapshot and native publication. Hold inner then inode publication across the complete export and use a helper accepting the held inner guard.

4. **High: submission errors can bypass process-context completion draining.** `ext4/migration.rs:77-78` uses `?` after earlier loop iterations may have submitted BIOs. These paths bypass `batch.wait_all()` and the completion queue drain. `kernel/libs/io-util/src/batch.rs` has no Drop implementation that waits. The last callback-held queue Arc can therefore destroy queued page leases in IRQ context. Funnel every error after submission starts through a wait/drain path, or use a persistent completion worker retaining ownership.

5. **Medium/high: durable SSD allocation has an untracked failure window.** `ext4/migration.rs:49-54` allocates and persists an SSD block before assigning its identity to the persistent page descriptor. A failure/crash in between leaves a durable allocated block with no retriable target identity; retry can allocate another. Requires an explicit ownership/recovery or safe cleanup protocol, including ambiguous target-persistence failure. No claim that ordinary successful I/O demonstrates this boundary.

6. **Spec mismatch: rNVPCe migration uses a DRAM bounce and synchronous per-filesystem pipeline.** `ext4/migration.rs:75-78` copies NV bytes to a stack page and allocated DMA segment. The approved physical-view BIO contract forbids this extra data plane. The same worker holds `nv_migration_lock` across `batch.wait_all()` and drains a per-round queue rather than using a persistent bounded completion worker. This is new integration code, not a historical limitation of the native filesystem.

7. **Recovery/export coverage needs proof.** `export_nv_for_sync` enumerates only `nv_inodes`, populated by live reads/writes. Recovered-but-untouched NV inode roots must also populate export discovery, or explicit sync/unmount misses their SSD-selected metadata. This is a source-level coverage concern; no recovery smoke evidence was available.

## Positive evidence and limits

- Persistent WAL capacity is 1024. `InodeCowContext::update` checkpoints before accepting the next update; checkpoint iterates the complete records vector without key coalescing and advances head only after tree replay/root publication.
- SSD completion has no call advancing persistent WAL head. Candidate filtering excludes `wal_pinned` entries, and completion validates exact page generation.
- Main restored WAL/tree/COW locks are sleeping `ostd::sync::Mutex` (including the kernel prelude Mutex), not spin mutexes around persistence. The shared adapter closes metadata-reader admission and drains pins before changing working bytes.
- `recover.rs:153-187` separates typed page-cache owners from rNVPCe objects, rejects mixed ownership, and scopes rNVPCe orphan reclamation to managed owners. rNVPC catalog validates expected frames/root before lazy replay.
- Anchor append failures after selector exposure poison the WAL and retain the new data page. The initially observed anchor-creation cleanup concern was corrected during review: WAL frames are now retained when anchor allocation reports ambiguous failure (`wal.rs:105-109`).
- Actual native VFS read/write dispatch reaches distinct rNVPC and rNVPCe implementations; selected modes reject an absent global pool instead of silently falling back.
- Restored legacy `extend_context` remains inode-only keyed and spinlocked, but the active rNVPCe EXT4 write path uses `commit_range_write`, not this legacy context. Do not mislabel that unused legacy interface as proof the new production route is cross-filesystem safe or unsafe.
- Normal DRAM fast persistence is not real PM crash durability. Required tests still include full 1024/1025 replay, repeated keys, split/root/selector fault boundaries, failed migration, live lease races, user-copy prefixes, and disposable-image read/write/sync/unmount. Final approval depends on runtime evidence and resolution of the high findings above.

## Final runtime evidence from integrator

`ktest-04.log`: 56 passed, 0 failed, 678 filtered out; exit 0. `build-final.log`: final make kernel exit 0 and ISO generated. The allocator/target persistent ownership gap remains unresolved despite runtime error latching. Full details: WAL-EXT4-INTEGRATION.md.
