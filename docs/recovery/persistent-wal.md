# Persistent WAL restoration and integration

The restored production path is `PersistentInodeWal` + `PersistentBtree` +
`InodeCowContext`. It is separate from the legacy DRAM cost-model types retained
in wal.rs for compatibility with old tests. rNVPCe does not use these objects.

## Source evidence

Evidence root: `/home/zn/zn/recovery_Rust_EXT4/research_recovery_20261016`.
Record numbers below are zero-based lines of `new-session-records.jsonl`.
They are session evidence, not available historical Git blobs.

- `pcache_store.rs`: complete command output record 11509 (2026-09-28
  12:44:14 UTC), 311 lines before formatting.
- `pcache_layout.rs`: numbered complete-prefix output 2901 (08:13:19),
  suffix matched exactly against record 398. Later selector-repair trait and
  node implementation restored from the original patch inside record 11807.
- `persistent_btree.rs`: latest prefix records 4833 and 4828 (14:02),
  overlapping suffix fragments 10685, 3444, 8252, 8247, 8253. Exact overlap
  required at each splice. Trailing output from another file was removed.
- `wal.rs`: latest prefix 4825 (14:01), exact overlaps 3444 and 8249.
  Trailing cow output was removed.
- `cow.rs`: prefix 4829 and overlap 4835 (14:02), old suffix 8252 with
  the recovered-orphan/cache rebuilding repair from diff 1842 applied.
  Exact owner/generation orphan reclaim follows the later 13:25–13:38 fixes.
- Final metrics changes for cow, WAL and tree restored from diff 3542
  (14:18:39). All its owned-file hunks applied without rejects.

Extraction scratch files: `/tmp/persistent-wal-evidence`,
`/tmp/reconstructed-pcache`, `/tmp/reconstruct_pcache.py`. The original evidence
was read only. This restoration is not a claim that the final files match an
unavailable original target blob.

## Explicit integration changes

- Added test-only RAII guard tails for existing checkpoint/mutation test hooks;
  complete historical tails were not present in recovered views.
- Added standalone production-code codec tests; two torn-selector tests failed
  with EINVAL before adding bounded valid-image fallback and anchor selector
  repair. A valid selector remains the publication boundary; a newer inactive
  image is not prematurely published.
- Replaced spin mutexes covering metadata persistence or test waits with
  sleepable mutexes in production cow/tree/WAL runtime paths. The public
  checkpoint entry now acquires the inode mutation lock; already-serialized
  callers use its private locked implementation.
- Corrected flush budgeting to filter WAL-pinned pages before taking the budget.
- Cow reads pin the exact committed allocation generation and inode owner.
- Final SSD reclaim propagates free errors rather than returning false success.

## Validation so far

- All restored files parse with the container Rust 2024 rustfmt.
- `rustc --edition=2024 --test docs/recovery/persistent-layout-harness.rs`
  followed by the produced test binary: **4 passed**. The preceding RED run had
  exactly the two torn-selector failures. This harness uses the real codec
  source and a minimal errno/prelude adapter; it proves neither whole-kernel
  behavior nor persistence on physical PM.
- `persistent_tests.rs` was added before the production restoration. It checks
  exact 1024/1025 boundaries, repeated-key replay count, SSD-completion WAL
  independence, WAL-pinned eligibility, failed checkpoint preservation/retry,
  eligible-page budgeting, and finalization error propagation. Kernel execution
  remains pending the parent integration build; these tests are not reported as
  passing merely because they compile.

## Invariants and remaining audit

WAL records are 64 bytes, 64 per 4096-byte frame, sixteen frames/inode. Data is
persisted before a committed record; anchor publication precedes the DRAM map.
Write 1025 synchronously replays every one of the 1024 records in order.
Checkpoint publishes the tree and anchor before releasing WAL capacity.
SSD completion removes the exact tree leaf mapping before reclaiming its data.
Recovery needs the parent-owned typed-object catalog and must preserve all WAL
and tree-referenced allocation generations when rebuilding shared pool state.

Whole-kernel compilation and booted crash/restart tests are still required.
The recovered split/fault/recovery implementation has source evidence but is
not assumed correct solely from restoration. Model persistence is not physical
PM crash proof.

Additional current validation coverage: eight kernel tests now include modeled
restart with repeated keys, ambiguous append retention, checkpoint splits with
100 distinct keys, and a five-cut root-split/republication crash matrix. Each
fault cut must return EIO before modeled restart reads all 71 mappings. They
remain unexecuted until the integration kernel can boot. Final standalone codec
output is retained in `persistent-layout-test.log` (4 passed).

A further failure-path correction retains WAL frames if initial anchor commit
reports an error: its descriptor may already be durable. Deleting the frames
in that case would leave a committed anchor with dangling references. Retention
can leak until recovery, which is preferable to losing the graph. The initial
root is likewise retained on failed context creation. Creation-failure injection
coverage has not yet been added.

Read leases propagate EBUSY to the EXT4 retry layer; they do not wait while an
EXT4 caller may hold its inode lock. The caller must release that lock before
retrying, allowing SSD completion to finish mapping removal.

## Booted validation progress

The parent `ktest-02.log` run executed the kernel suite: 45 passed and 3 failed.
Five persistent-WAL tests passed, including foreground 1025 replay, checkpoint
failure/retry, eligible-budget filtering, finalization error propagation, and
multi-leaf checkpoint mappings. The three restart tests failed before context
recovery because the test fixture used `Rnvpc::new`, which does not persist an
image header. The fixture now calls `Rnvpc::format`; restart tests await rerun.
The root-fault matrix was rechecked against injection sites: a 70-entry full
leaf splits on its 71st distinct key, so each selected fault is reachable; the
fixture has 128 data slots and enough metadata capacity.
