# EXT4 integration recovery and implementation

Worktree: `os_com_rnvpc_rebuild`. Evidence/checkpoint directories remain read-only.
Authoritative mechanism: `Rust_EXT4/os_com_implementation/rNVPC-rTIO/design.md`,
foreground checkpoint design, and `os_com_implementation/STATUS.md` (2026-10-13).

## Evidence inspected

- `research_recovery_20261016/{full-version-index,refined-version-index,structured-file-changes,positioned-views}.json`:
  complete EXT4 fs/inode sources are only the old baseline; 57 later EXT4 patches
  and partial positioned observations remain. They do not establish a complete
  latest fs/inode source.
- Raw candidate catalog `os_com_candidates_20261015T074727Z/unique-candidates.tsv`.
  Substantial raw pieces are shuffled/truncated; balanced braces alone did not
  establish valid function recovery. Every used piece was inspected for joins.
- `7a352bd790886f998c9d9caf939f2068e101fec1614ce54da7875df455eb5d2f`:
  intact optional `LatencyStat`, `read_ns`, disabled throughput-mode switch.
- `7a3f4249a243e7a6c07f94c1c539833353e883b159811937fbd44336eb5b09d5`:
  completion stage constants/setter, partial stats/fs sections.
- `ac4067d34838bd24d40edf65ae0c371bc59d897ae81c81fd1f3b78f0d3c0a3ad`:
  mount mode resolution and SSD count-reservation functions.
- `4f16ebc10eda17a2f6328ef0d3e4919f689995387095d832da78a4112c5d0883`:
  intact direct `read_rnvpce_at`, inode-field restore and native export helper.
- `3b1d4074f52036e2e6f3e9c70f9adce98bec8b797ef888c4008b6f51db4d2c9a`:
  foreground authority bootstrap/page-span construction; tail was torn and
  reconstructed against the restored `RangeWriteBatch` API.
- `8000aeca7dfe926c44ea070ddae15a6fcb3aa296bc3dcb0f93ed913163193e11`
  and `6abc29917f54887afb5c13de319d11febd89eef94613bd918c65957154d65640`:
  partial native extent install/snapshot methods, completed against baseline
  tree insert/walk APIs.
- `59a8365e07df`, `751a7865d279`, `adcd40a276d5` raw function fragments:
  weak-owned workers, bounded IRQ ownership transfer, process-context durability
  and generation-checked reclaim. Their old native-completion materialization was
  superseded by the latest design's deferred filesystem-sync export.

`stats.rs` consolidates recovered recorder logic and referenced fields; it is
not a byte-identical recovered original. `migration.rs` reconstructs the current
contract over the restored APIs; it is not claimed as an exact source recovery.

## Implemented call chains

- Mount options (`rnvpc`, `rnvpc_extend`/`rnvpce`, `rtio`, on/off values) resolve
  mutually exclusive routes, reject conflicting modes and absent pool capacity.
- Native EXT4 follows its existing read/write path. Both NV routes handle direct
  I/O through their own authority and deny native page-cache mmap exposure.
- rNVPC obtains filesystem-scoped persistent WAL/tree contexts, composes the
  successful user prefix against NV/SSD data, and publishes via the persistent
  WAL update API with actual allocated SSD targets. Migration eligibility is
  controlled by WAL checkpoint state.
- rNVPCe bootstraps fixed inode/extent authority, copies user bytes directly into
  NV pages, reserves SSD count before publishing new mappings, and reads direct
  NV references or SSD-selected persistent extents. SSD partial-page initialization
  DMAs directly into a private NV page.
- Separate weak-owned submission and completion workers handle rTIO. Submission
  claims generation-specific page leases and concrete EXT4 SSD blocks, and merges
  contiguous targets into scatter/gather BIOs over physical NV page segments.
  The bounded IRQ queue owns completion items; IRQ callbacks only append/wake.
  Process completion batches durability, groups pages by inode for leaf-local
  publication, releases reservation credit, and reclaims exact generations.
  In-flight credit returns after process draining; teardown drains all submitted
  ownership before dropping the queue.
- rNVPCe data completion never installs native extents. Explicit filesystem sync
  (also the existing VFS unmount hook) drains migration and exports SSD-selected
  extents under inode-inner then publication exclusion. It retains NV metadata.
  Per-file rNVPCe fsync does not force SSD/export.
- Resize, fallocate, regular-file unlink and rename-overwrite reject NV modes
  until persistent graph mutation/reclamation exists; native behavior remains.

## Validation and remaining limits

Added `nv_mode_tests` and `nv_tests` cover mode conflicts, WAL vs WAL-free routing,
partial direct write/read, busy-lease read/write retry without duplicate WAL append, per-file sync without SSD flush, mmap exclusion,
filesystem sync migration/export, and native behavior. Route tests explicitly initialize a shared Fast 64MiB test pool (32MiB data and
32MiB metadata) with a cfg(ktest) helper; they never silently skip. Full build and actual QEMU results are recorded by the parent agent;
no runtime success is implied by this document.

The reconstructed allocator-to-NV-target handshake has an unresolved crash
window: EXT4 allocation can become durable before the descriptor target becomes
durable. Before target publication is attempted, runtime durability failure rolls the private
SSD allocation back through the ordinary free transaction. After target publication
is attempted, ambiguous failure latches migration off until recovery and
conservatively retains allocation/credit rather than risking
reallocation of a possibly named block, but a recovery ownership ledger spanning
this gap is not yet demonstrated. This must not be described as leak-free crash
recovery. Error-path admission charges may likewise remain conservative until
mount recovery rebuilds them. Device failure can prevent unmount draining, as
with outstanding ordinary block I/O; no forced timeout discards owned NV leases.

Persistence is modeled by the configured NV backend; builds/in-memory fixtures
cannot establish real-PM power-failure behavior or hardware bandwidth.

The shared-pool test run exposed generic rNVPCe metadata decoding being applied
to typed rNVPC WAL pages. Owner/schema filtering now precedes generic decoding;
mount rejects switching an existing filesystem's authority scheme (including a
native mount over retained NV ownership). Different filesystem IDs may share the
pool. Added regressions cover both switch directions, failed pre-WAL device
barriers (zero WAL appends), retry, and native inode size durability after success.
Background submissions now explicitly require the high-water mark; filesystem
sync remains an unconditional drain/export request.
