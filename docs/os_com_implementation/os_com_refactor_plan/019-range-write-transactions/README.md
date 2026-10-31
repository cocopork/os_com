# 019: rNVPCe Range Write Transactions

Date: 2026-10-31. Status: Implemented; final regression and device validation in progress.

## 1. Intent

Native rNVPCe currently accepts a range write but expands it into one
`RangeWritePage` per 4 KiB page. A contiguous 128 KiB overwrite consequently
repeats extent routing, redirect lookup, version reservation, slot locking, and
runtime-index publication for 32 `PageKey` values before publishing one
`NvRedirectExtentRecord`.

The redesign makes the NV redirect extent entry the minimum authority,
transaction, concurrency, migration, and reclaim unit. `PageKey` remains a
coordinate used to locate an entry; it is no longer an independently versioned
Native redirect transaction.

The target complexity is proportional to the number of range discontinuities,
not the number of 4 KiB pages. A logically, NV-physically, and SSD-physically
contiguous 128 KiB overwrite must use one range route, one NV allocation range,
one redirect entry, and one entry-level publication transaction.

## 2. Goals

1. Plan one user write as a small set of maximal `WriteExtent` ranges.
2. Route the SSD extent tree and volatile redirect index once per range, not
   once per page.
3. Publish and version Native redirects per `NvRedirectExtentRecord`.
4. Acquire write/migration ownership once per overlapping redirect entry.
5. Persist all newly initialized data ranges, execute one data fence, publish
   all redirect selectors, then execute one publication fence.
6. Preserve the entry-level migration, completion, recovery, and reclaim
   protocol introduced by plan 018.
7. Make a contiguous NV slot range one runtime allocation object so allocation
   and reclaim do not reacquire one `ObjectTable` lock per page.

## 3. Non-goals

- A multi-page `write(2)` is not made failure-atomic. A crash before the system
  call returns may expose a valid prefix of complete redirect entries.
- The native EXT4 SSD extent format and FAST/FAIR tree-split protocol do not
  change.
- The rNVPC WAL mode and persistent metadata-object descriptors do not change.
- The virtio queue, rTIO worker count, and SSD block-group allocator are outside
  this change.
- Adjacent dirty entries are not synchronously coalesced on the foreground hot
  path. Coalescing is optional follow-on work.

## 4. Range model

The planner produces:

```rust
pub(crate) struct RangeWritePlan {
    inode_generation: u64,
    file_offset: u64,
    write_len: usize,
    first_page_offset: usize,
    last_page_end: usize,
    extents: Vec<WriteExtent>,
}

pub(crate) enum WriteExtent {
    ExistingNv {
        entry: Arc<DirtyNvExtentEntry>,
        logical_range: Range<u64>,
    },
    ExistingSsd {
        logical_range: Range<u64>,
        ssd_start: u64,
    },
    NewAllocation {
        logical_range: Range<u64>,
    },
}
```

One range walk splits only at semantic discontinuities:

- an existing redirect-entry boundary;
- an SSD extent physical discontinuity, hole, or unwritten boundary;
- the old EOF and inode visibility-generation boundary;
- an NV allocator range/domain boundary;
- the maximum 128-page redirect-entry length;
- a migration-owned entry that requires the writer to wait and replan.

Unaligned writes may have two partial boundary pages. Fully covered interior
pages do not perform individual mapping validation. A partial overwrite of an
existing dirty NV entry acquires one entry writer reference, changes only the
requested bytes, and does not split the entry.

## 5. Volatile redirect range index

Native redirects use an inode-owned interval map rather than independent
`PageKey -> DescriptorHandle` transactions. The outer partition is selected
only by `(fs_id, inode)`, so an arbitrary member page can locate the inode map
without already knowing its entry start:

```rust
struct RedirectDirectory {
    partitions: Box<[SpinLock<BTreeMap<(u64, u64), Arc<InodeRedirectMap>>>]>,
}

struct InodeRedirectMap {
    entries: SpinLock<BTreeMap<u64, Arc<DirtyNvExtentEntry>>>,
}
```

The directory exposes these operations:

```rust
fn lookup_page(&self, key: PageKey) -> Option<Arc<DirtyNvExtentEntry>>;
fn lookup_overlaps(
    &self,
    fs_id: u64,
    inode: u64,
    range: Range<u64>,
) -> Result<Vec<Arc<DirtyNvExtentEntry>>>;
fn publish_entries(&self, entries: &[Arc<DirtyNvExtentEntry>]) -> Result<()>;
fn remove_entry(&self, entry: &Arc<DirtyNvExtentEntry>) -> Result<()>;
```

The directory may retain fixed outer lock partitions, but all entries of one
inode are found through one `InodeRedirectMap`. Page lookup performs one
predecessor search and bounds check. It must not install one membership node per
page. Entry identity is the DRAM `Arc`; stale completion or reclaim work cannot
rediscover a reused allocation by slot number.

Native data redirects no longer use per-page `logical_version`, `pending`
version sets, committed handles, or data-page `allocation_seq`. Those mechanisms
remain available only where rNVPC WAL or persistent metadata objects require
them.

## 6. Foreground protocol

The foreground path uses the following phases:

```text
F0  Snapshot inode authority and range-route SSD/redirect mappings.
F1  Reserve the exact number of new NV slots outside publication/entry locks.
F2  Acquire the inode publication right once and entry writer references in
    logical order; validate the inode generation and entry Arc identities.
F3  Allocate maximal contiguous NV ranges; preallocate every Arc entry, inode
    map, publication vector, and inactive redirect record needed after commit.
F4  Copy user bytes. Read SSD only for partial boundary pages.
F5  Persist all changed/new NV data ranges and inactive range records; fence.
F6  Publish all new range selectors; fence.
F7  Atomically install the preallocated entries in the volatile range index
    using a no-allocation, infallible state transition; release entry writers.
F8  Publish inode size/block-count/root once, and only when they changed.
```

No sleep, capacity wait, SSD I/O wait, or device flush may occur while holding a
spinlock. If F2 observes a changed generation, replaced entry, or migrating
entry, it releases all acquired state, returns the reservation, waits if
necessary, and replans from F0. Revalidation is once per planned entry, never
once per member page.

Existing NV entry overwrite does not republish redirect metadata. It persists
the modified byte ranges and executes one fence before returning. Existing SSD
or hole ranges publish new redirect entries using F3--F7.

A copy fault or short user copy truncates the transaction to the completely
initialized prefix. A partial boundary page may be published only after its
untouched bytes have been loaded and the complete page has been persisted. No
entry may describe an uninitialized suffix.

## 7. Persistence and crash interpretation

For new redirect entries:

| Crash point | Recoverable interpretation |
| --- | --- |
| Before F5 fence | Old mapping remains authoritative; uncommitted slots are staging leaks and are reclaimed by recovery. |
| After F5, before a selector is durable | Initialized data/record is unreachable and reclaimable. |
| Between selector publications | Every durable selector names one complete entry; the unreturned write may have a valid prefix. |
| After F6 | All published entries are independently recoverable as NV authoritative. |
| Before inode size publication | Entries tagged with the uncommitted visibility generation are ignored and reclaimed. |
| After inode publication | The new file range is fully initialized and reachable. |

The soft-update ordering is therefore data/record before selector, and selector
before inode reachability. A successful return requires both fences and any
required inode publication to have completed.

`NvRedirectExtentRecord` image version is bumped and its existing `reserved`
word becomes `visibility_generation`. An overwrite wholly below the committed
EOF uses the currently committed inode generation. A first allocation or file
extension uses the target inode generation and is split at the old EOF. Recovery
accepts a redirect record only when its visibility generation is not newer than
the authoritative inode generation. This prevents F6--F8 crashes from exposing
an extension that the inode never published.

All volatile insertion storage is reserved before F5. Code after a durable F6
must not allocate or return an ordinary error. An invariant violation after F6
latches `publication_uncertain`, blocks further access, and requires recovery;
it must not continue with a missing volatile redirect.

The implementation represents this ordering explicitly. New DRAM entries are
inserted in `Publishing` state before selector persistence and cannot satisfy a
lookup. After selector durability, publication is an allocation-free,
infallible `Publishing -> Dirty` transition. If persistence fails before the
result becomes uncertain, the prepared entries and range owners are removed.
If durability is uncertain, the pool is latched for recovery instead of
guessing which selectors reached NVM.

The on-media pool version is 9. Recovery durably clears selectors whose
`visibility_generation` is newer than the authoritative inode generation
before returning their slot ranges to the allocator. Generation zero is
reserved for the legacy per-page redirect compatibility API; Native records
always use a nonzero inode visibility generation and recover only as range
owners.

Plan 018's reclaim ordering remains unchanged:

```text
SSD device flush
  -> entry authority = SsdAuthoritativePendingFree; persist
  -> exclude/wait readers
  -> clear the entry head selector; persist and fence
  -> return the complete NV slot range
```

## 8. Range-owned slot state

`ObjectTable` currently protects every slot with an independent lock. Native
redirect data adds a range-owned runtime record:

```rust
struct DataSlotRange {
    start: usize,
    len: u16,
    state: DataRangeState,
    owner: Weak<DirtyNvExtentEntry>,
}
```

The minimum range-owner API is introduced before range publication replaces
per-page handles:

```rust
fn begin_reserved_data_ranges(...) -> Result<Vec<StagingDataRange>>;
fn begin_range_data_mutation(
    entry: &Arc<DirtyNvExtentEntry>,
    logical_range: Range<u64>,
) -> Result<RangeDataMutation>;
fn install_data_range_owner(entry: &Arc<DirtyNvExtentEntry>, slots: Range<usize>) -> Result<()>;
fn remove_data_range_owner(entry: &Arc<DirtyNvExtentEntry>) -> Result<Range<NvPageId>>;
fn data_range_segments(
    entry: &Arc<DirtyNvExtentEntry>,
    offset: usize,
    count: usize,
) -> Result<Vec<USegment>>;
```

Allocation, migration claim, and reclaim update one range record under one
allocation-domain lock. Data access pins the owning entry through its reader or
writer guard and calculates `start + logical_offset`; it does not pin every
slot. The range is not visible to the allocator until the persistent selector
has been cleared and all entry readers have exited.

Persistent metadata frames, rNVPC WAL data, and compatibility test APIs retain
their existing per-slot object records. The Native hot path must not mirror a
range back into 32 authoritative per-slot identities.

The implemented Native reservation path charges capacity and allocates slots
as ranges without initializing per-slot `ObjectRecord` values. One persistent
redirect head and one DRAM range owner are the complete Native identity.
Reclaim clears that head selector, removes the range owner, and returns the
whole allocator interval. The generation-zero compatibility path keeps the old
per-page lifecycle so rNVPC/WAL tests and old helper APIs remain isolated.

For a short user copy after SSD blocks have been reserved, only the initialized
prefix may become visible. The unused SSD suffix remains covered by its
persistent allocation-ownership record until EXT4 releases the unpublished
blocks. Only after that release succeeds may the allocation record be
reclaimed; cleanup failure is therefore a recoverable leak, never a duplicate
allocation.

## 9. Concurrency

- Foreground writes take one writer reference for each overlapping dirty entry.
- `Dirty -> Migrating` prevents new writers and waits for existing writers to
  leave before rTIO reads the immutable range.
- Reads remain allowed in `Dirty`, `Migrating`, and `IoComplete`; reclaim blocks
  new readers only at `Reclaiming`.
- Locks for multiple entries are acquired in `(fs_id, inode, logical_start)`
  order. Allocation-domain locks are never nested with redirect-index locks.
- Different entries, including entries of the same inode, may proceed
  independently once the existing inode-wide foreground serialization is
  relaxed by a later change. This plan does not require that relaxation.

## 10. Acceptance

Correctness tests must prove:

1. A contiguous 128 KiB overwrite produces one plan extent, one redirect entry,
   and no per-page Native descriptor/version transaction.
2. SSD, NV allocator, hole/unwritten, entry, and 128-page boundaries split only
   at the corresponding discontinuity.
3. A 4 KiB overwrite inside an existing 128 KiB dirty entry does not split or
   republish the entry.
4. A writer racing `Dirty -> Migrating` waits outside locks and replans without
   changing migration data.
5. Fault injection after every F5/F6 selector boundary recovers only initialized
   NV data or the old SSD mapping, never a dangling slot.
6. Recovery rebuilds one complete range entry from one head selector.
7. Reclaim clears one selector and returns one complete slot range.
8. Requests larger than 128 pages are completed by an internal range loop and
   never leak a short write to userspace solely because of the entry limit.

## 11. Implementation mapping

- `write_transaction.rs` plans and commits bounded range transactions and
  retains short-copy allocation ownership until cleanup.
- `redirect_index.rs` provides the inode interval index, entry-level
  reader/writer state, and prepared publication transition.
- `frame.rs` owns Native capacity and slots by range; `frame/redirect.rs`
  publishes and reclaims one range identity.
- `recover.rs` rebuilds reachable range owners and removes selectors that are
  newer than the authoritative inode.
- `rnvpce_integrity_tests.rs` covers selector crash boundaries, stale
  generations, range reuse, large-write chunking, and short-copy allocation
  cleanup.
9. F6--F8 crashes do not expose a redirect whose visibility generation is newer
   than the authoritative inode generation.

Selector-boundary tests use a counted test-only fault specification rather than
one enum trigger:

```rust
struct FaultSpec {
    point: FaultPoint,
    occurrence: usize,
}
```

Production builds compile out occurrence accounting.

Performance validation uses the existing 2 GiB workload, 128 MiB NV pool, and
64 MiB data limit. It runs 128 KiB sequential overwrite at jobs 1 and 16 for at
least three alternating rounds, then records range routes, planned entries,
entry-lock wait/hold, data fences, selector fences, capacity waits, rTIO
candidate pages, BIO sizes, and durable bandwidth. The expected structural
result is metadata work proportional to entries; a bandwidth increase is
measured, not assumed.
