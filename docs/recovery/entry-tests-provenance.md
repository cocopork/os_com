# Entry module and test subset provenance

These are evidence-led reconstructed source files, not exact target blob recoveries. The existing complete old NVPool reference, positioned views, selected raw fragments, and dated session output were inspected before assembly. No original or evidence source was modified.

## mod.rs

The complete September 29 session view `/home/zn/asterinas-build-env/rnvpc-restoration/mod-session-views/570c790fe83df20f.txt` (command/timestamp in its sibling JSON) supplies the entry module's context tables, constructors, counter snapshot, watermark/request helpers, WAL-full discovery, and completion dispatch. Its line-number prefixes were removed.

The latest intact prefix in selected `fc881a10bb4a16257e32623f23ebb955f36a99bdb2559b111c651ac02138bf75__00068350.txt` replaces the module declarations and global-init/read-helper section. This includes `extent_index`, `extent_mutation`, `write_transaction`, recovery profiling, and mapped-page reads. The prefix begins at `config`, so `backend` and `capacity` declarations and the file header come from the complete session view. The raw latest declaration list does **not** contain `rtio`; no obsolete rtio module was inserted.

Selected raw `ba81f85d0764b5b6e0fdb5750330347a331583151cfb76eece0af4005726dc9a__00108885.txt` supplies the intact latest legacy `flush_candidates` (COW contexts only) and `RnvpceFlushCandidate` structure. Its final dirty-queue function was truncated. An additional raw candidate supplies that function in full:

`/home/zn/zn/recovery_Rust_EXT4/os_com_candidates_20261015T074727Z/candidates/02_project_rust/59/59dbaee6537e8cc590ea39f5567d031fa522ccaf36cd2ed2b206967af2bfd98c__00079032.txt`

That body already contains the October 9 `migration_is_frozen` retry change from `mod.rs.changes.json`; it is copied without inventing the missing tail. Candidate selection validates allocation sequence, filesystem, reverse locator, migration freeze, and SSD backing/ownership. rNVPCe discovery does not walk inode extent trees.

Version conflict: September 28 checkpoint-branch session views contain tuple-keyed COW tables and prepare/finalize completion, whereas the later September 29 view, latest raw fragments, and surviving `wal.rs`/recovered `cow.rs` describe inode-keyed DRAM WAL cost-model contexts and completion retirement. The latter coherent code line is restored; this does **not** implement the final design's persistent foreground WAL checkpoint contract. The inode-only context key remains a documented single-filesystem limitation.

A known inherited observability defect remains: `snapshot()` returns early if no COW table exists, before collecting extend-only counters. The intact session implementation and latest partial raw suffix both exhibit this behavior. It was not silently rewritten during evidence restoration.

## tests.rs

This file is explicitly a **25-test included subset**, not the recovered full original test suite. It contains:

- Seventeen complete tests from `/home/zn/zn/Rust_EXT4/asterinas_ext4/kernel/core/src/fs/nv_pool/tests.rs`: configuration, IDs, segmented backend access, persistence counters, capacity/watermarks, pins, data lifecycle, and staging. The shared `test_pool` helper is recovered from the same source. NvPool names are mechanically adapted to Rnvpc.
- Eight compiled tests from `/home/zn/asterinas-build-env/rnvpc-candidate-search/tests.rs.changes.json`: reservation accounting and dirty-queue/reverse discovery (October 9 10:29:36), extent-reader/writer exclusion (October 11 02:45:18 and 03:06:10), publication-context isolation (October 12 03:02:24), normal crash-model rejection (October 12 05:47:50), failed/parallel reclaim accounting (October 12 09:25:41); the later waiting leaf-mutator test is preserved separately as described below.
- Follow-up patches incorporated: dirty-queue test's range-freeze assertions (October 9 10:45:21); BTreeSet spelling in parallel reclaim (October 12 09:27:38).

Only complete added function bodies were extracted from patches; unrelated context and later partial functions were not treated as complete tests. The test imports are assembled against those bodies and current APIs. Omitted portions include the unrecovered full WAL/CoW, crash/restart, extent mutation, batch write/migration, and fixed-inode test matrix. Their absence is a recovery coverage gap, not evidence those mechanisms work.

The five additional focused tests in config.rs/dram.rs are recorded separately in backend-provenance.md. The restored tests are intended for the parent integration ktest run. Rustfmt parsing is syntax evidence only; runtime success must be recorded by the integration validation report.

## Integration-driven version corrections

The first ktest typecheck exposed two historical-version mismatches. The reservation test is replaced by the complete matching-version body from raw `02_project_rust/bf/bfcc70a3d3c6d44e3225887eb7e355d0c732b42836213b0f2c68e59e03787c16__00086344.txt` (lines 2574–2598). It acquires `ExtentMutation`, passes it to both reserve/consume APIs, and additionally asserts one in-place metadata record. No production API was changed.

`rnvpce_metadata_mutation_guard_wakes_a_waiting_leaf_mutator` requires the absent later `Rnvpc::lock_metadata_mutation` API. Its complete recovered body is retained in `later-tests/metadata_mutation_guard.rs.reference`, with provenance in that directory's README. It is not part of the compiled suite; this is an explicit coverage/version gap. The tests.rs compiled count is now 25; the five local config/dram tests remain separate.
