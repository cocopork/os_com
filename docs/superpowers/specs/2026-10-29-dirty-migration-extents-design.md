# Dirty Migration Extent Index and Range Migration

Date: 2026-10-29

Status: Implemented and measured on 2026-10-29.

## 1. Intent

rTIO must select and migrate dirty NVPages without scanning every dirty page,
globally sorting candidates, or repeatedly acquiring page-level locks.  The
selection unit is a **dirty migration extent** (脏迁移区段): a currently
logical-contiguous and NVPage-physical-contiguous range of authoritative dirty
pages.

For example, if the confirmed SSD baseline is `[0, 8)` and pages `[2, 5)` are
redirected to consecutive NVPages, the dirty migration extent is `[2, 5)`.
It is not the original SSD extent and does not split that baseline extent.

Each indexed dirty migration extent is limited to **128 pages**.  This is a
structural invariant, not merely an I/O batching hint.

## 2. Invariants

1. One extent contains 1 through 128 pages.
2. Its pages have the same filesystem and inode, consecutive logical page
   numbers, consecutive NVPage slots, compatible dirty authority state, and
   belong to one index partition.
3. A 129-page compatible range is represented as at least two extents.  Local
   merge and split operations must never create a longer extent.
4. The foreground first-write transaction publishes the SSD baseline before
   returning, but the persistent per-page descriptor remains the data authority
   until SSD I/O completes and that exact descriptor generation is cleared.
5. Candidate claim and SSD I/O state are DRAM-only.  Selection must not persist
   `NvDirty -> NvWriteback` for every page.
6. A claimed extent blocks foreground updates only in its logical range.  Other
   ranges, including other ranges of the same inode, remain writable.
7. No index-partition spin lock, inode lock, or NV allocator lock may be held
   while waiting for SSD I/O, capacity, or migration completion.
8. Recovery does not resume DRAM migration tickets.  It rebuilds the dirty
   extent index from authoritative selected descriptors and migrates them
   again; repeated SSD writes must therefore be idempotent.

## 3. Runtime Model

```text
selected descriptors
        |
        v
per-partition ordered dirty-extent index (each extent <= 128 pages)
        |
        +--> ready queue --> MigrationExtentLease --> target allocation/BIO
                                                    |
                                                    v
                                            MigrationExtentTicket
                                                    |
                                                    v
                                      completion + parallel reclaim
```

Use “index partition” / “索引分区” in code and documents.  Do not use `shard`.
The partition lock protects only ordered-map and ready-queue mutation.  The
long-lived exclusion is represented by a DRAM lease record, not by retaining
that lock.

The core records are:

```rust
const MAX_DIRTY_MIGRATION_EXTENT_PAGES: usize = 128;

struct DirtyMigrationExtent {
    fs_id: FsId,
    inode: InodeId,
    logical_start: u64,
    len: u16,                 // 1..=128
    nv_base_slot: NvPageId,
    descriptors: Vec<PersistentObjectRef>,
}

struct MigrationExtentLease {
    extent_id: DirtyExtentId,
    generation: u64,
    nv_slot_range: NvSlotRangeLease,
    wait_key: MigrationWaitKey,
}
```

The exact descriptor handles are captured when an extent is claimed.  They are
the completion identity; migration does not route through the extent tree or a
reverse `NVPage -> extent` lookup.

## 4. Incremental Index Maintenance

Descriptor publication, replacement, clearing, recovery, and state changes
update only the affected logical neighborhood.  The implementation keeps
deduplicated ready page keys and materializes a bounded extent lazily when a
key is selected; it does not eagerly reconstruct every extent after an update:

- insert a page and merge left/right only when all invariant fields match;
- cap a merge at 128 pages, leaving the remainder as an adjacent extent;
- replacing or clearing an interior page splits the old extent into at most
  two remaining extents;
- an NV slot discontinuity always creates a boundary;
- claimed extents are removed from the ready queue but remain searchable in a
  separate interval map for foreground conflict detection;
- failed or stale claims are locally rebuilt from their exact descriptor
  handles and requeued without a global scan or sort.

Recovery scans selected descriptors once and rebuilds each partition's ready
keys.  This startup work is allowed to be global; the steady-state candidate
path is not. Completion may aggregate several separately locked 128-page
extents for one descriptor-clear and NVPage-free batch. It does not publish
SSD baselines for normal writes. Aggregation does not enlarge an extent lease
or its foreground exclusion range.

## 5. Candidate Claim and Range Coordination

Candidate selection pops ready extent IDs from partition queues round-robin.
It does not collect every dirty page and does not globally sort candidates.
Each popped extent is checked once under its partition lock and converted to a
lease.  The lease generation prevents ABA if descriptor replacement later
recreates the same logical range.

The claim performs range-oriented operations:

1. reserve one dirty extent, snapshot its exact descriptor handles, and install
   its logical interval lease while holding the index-partition lock once;
2. acquire one DRAM physical-slot range lease for
   `[nv_base_slot, nv_base_slot + len)`; all descriptor replacement, clearing,
   and NVPage reuse paths must consult this lease;
3. batch-claim the confirmed SSD baseline generations for the whole range;
4. use the SSD targets and baseline mappings already published by the
   foreground first-write transaction;
5. reserve a group and allocate a target only for a recovered legacy descriptor
   that predates foreground target publication, then submit BIOs;
6. create a ticket containing the lease, exact descriptors, targets, and
   baseline generations.

The logical and physical range leases replace per-slot object pins in this
path.  They use one short critical section each and never retain a spin lock
over I/O.  Baseline claims use one range/batch API.  If validation fails,
release the whole claim, locally reconstruct only that dirty extent, and
requeue it.

An allocation group may participate in only one migration transaction at a
time.  Target allocation first acquires group ownership, then modifies the
NVM-authoritative group allocation metadata.  If no suitable group is
available, retain the extent ID in a deferred ready queue and wake it when a
group is released; do not repeatedly rediscover or persist the same pages.

## 6. Foreground Write Interaction

The foreground path queries the migration interval map in logical order.

- If the requested range has no overlap, publish redirects normally.
- If a conflict occurs after a writable prefix, commit that maximal prefix.
- Before waiting, release inode mutation, extent-tree, allocator, and capacity
  locks.  Wait on the lease's wait key, then re-query from the current logical
  position because the mapping may have changed.
- Do not skip a blocked middle range and write a later suffix out of order.
- If an error occurs after a prefix was committed, return the normal partial
  write result.

The existing 128-page foreground transaction limit matches the dirty extent
limit but remains a separate correctness boundary: writes may still be split
earlier by a migration conflict, NV physical discontinuity, or index partition
boundary.

## 7. Completion, Reclaim, and Failure

Completion first checks the ticket generation, exact descriptor handles, and
baseline generations. After the required device flush it marks the exact dirty
extent SSD-durable and publishes a reclaim ticket. The normal path does not
route or mutate the extent tree. Only then may the exact descriptors become
reclaimable or be cleared.

Parallel reclaim remains compatible with plan 017: completion may mark exact
NVPages reclaimable, while foreground allocators reclaim clean pages through
per-CPU caches.  The migration lease is released and waiters are notified only
after no new foreground write can mistake the old ticket for the current
authority.

On BIO, flush, validation, or publication failure, persistent descriptors stay
authoritative.  Release target allocations as allowed by the allocation
protocol, drop the lease, locally reconstruct the extent, and requeue it.

On crash, SSD data or metadata written by an incomplete ticket is not
authoritative.  Recovery trusts NVM descriptors, rebuilds the index, and
repeats migration.  Leaked SSD allocation is recoverable background cleanup,
not visible file data.

## 8. Acceptance Criteria

- Index construction and every incremental update enforce `len <= 128`.
- A compatible 256-page range becomes exactly two 128-page extents.
- Candidate selection performs no full dirty-page scan and no global sort.
- One claimed 128-page extent uses one partition claim, one physical-slot
  range lease, and one baseline batch claim rather than 128 repeated lock
  cycles.
- Foreground writes overlapping one claimed extent wait without inode or
  allocator locks; non-overlapping ranges of the same inode continue.
- Candidate claim performs no per-page persistent state transition.
- A busy allocation group does not cause repeated candidate selection or
  descriptor persistence.
- Completion uses ticket identity and exact descriptor handles, not extent-tree
  routing or reverse lookup.
- Crash tests at foreground baseline publication, claim, BIO, flush, and
  descriptor clear
  always recover either the old SSD baseline plus NV authority or the fully
  published SSD result, never a dangling authoritative SSD reference.

## 9. Non-Goals

- Persisting or resuming migration tickets across reboot.
- Changing rNVPC's WAL protocol.
- Reintroducing mixed SSD/NV targets into persistent extent entries.
- Using inode-wide publication exclusion.
- Adding locality-group preallocation beyond the group ownership required for
  orderless allocation metadata updates.
