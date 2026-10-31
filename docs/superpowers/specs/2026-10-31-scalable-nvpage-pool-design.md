# Scalable NVPage Pool Design

Date: 2026-10-31

Status: Implemented and validated on 2026-10-31. See
`docs/experiments/reports/SCALABLE-NVPAGE-POOL-2026-10-31.md`.

## Intent

NVPage allocation and reclamation must scale across CPUs without serializing
unrelated page ranges behind `Rnvpc::allocation` or `Rnvpc::state`. The design
must still support contiguous NV redirect extent entries of up to 128 pages,
preserve the existing data/metadata/WAL capacity policy, and reconstruct all
volatile allocation state after a crash.

The selected architecture uses fixed allocation domains and CPU-local domain
selection. A CPU normally allocates from its home domain and searches other
domains only when the home domain cannot satisfy the request. A small CPU-local
free-range cache is an optional later optimization, not the source of
allocation authority and not a permanent partition of the pool.

## Evidence and Scope

The current `reserve_data_pages()` path takes the single `allocation` lock,
sorts the complete `free_ids` vector for multi-page requests, searches for a
contiguous range, removes the selected IDs, and initializes every selected
slot before releasing the lock. Reclamation also takes `state` and
`allocation` while returning pages. `RnvpcExtend` redirect publication and
reclaim still update the legacy global `PoolState` index per page even though
`RedirectDirectory` is the `RnvpcExtend` logical-to-NV authority.

The measured 128 KiB sequential overwrite workload degrades more severely at
16 jobs than first-write, even though overwrite avoids most new SSD allocation.
This makes the NVPage pool locks a stronger current hypothesis than the SSD
block allocator. This is measured correlation, not yet a causal proof. Phase 0
instrumentation is required before implementation and remains enabled for the
comparative experiment.

## Goals

- Disjoint allocation domains allocate and reclaim concurrently.
- The normal allocation path does not sort or scan the complete pool.
- A request of at most 128 pages is returned as one physically contiguous
  range whenever any domain contains such a range.
- The allocator may split a larger logical request only when no domain can
  provide the complete range. Each returned part becomes a separate NV
  redirect extent entry.
- CPU identity affects placement only; thread migration cannot affect
  correctness or persistent ownership.
- Data and metadata objects continue to share the same physical slot namespace
  without maintaining conflicting free-space indexes. WAL continues to consume
  PM capacity accounting without allocating a physical pool slot.
- `RnvpcExtend` data allocation, redirect publication, completion, and reclaim
  no longer acquire a pool-wide data index lock.
- Allocation-domain state and CPU-local hints remain volatile. Recovery derives
  them from persistent object and redirect records.

## Non-goals

- permanently assigning a private NVPage pool to each CPU;
- changing the persistent NVPage format solely to encode CPU ownership;
- changing rNVPC WAL semantics or removing the legacy index from modes that
  still use it as their logical lookup authority;
- allowing a CPU-local cache to retain an unbounded fraction of the pool;
- persisting allocator search trees, per-CPU cursors, or allocation-domain
  locks;
- changing the 128-page maximum NV redirect extent entry in this work.

## Terminology

An **allocation domain** is a fixed, contiguous range of physical NVDIMM slots
with an independent lock and free-space index. Slot ownership by a domain never
changes at runtime.

A **CPU-local domain hint** records the home domain, last successful domain,
and next fallback position for one CPU. It owns no pages.

A **CPU-local free-range cache** is an optional bounded cache of free contiguous
ranges removed from an allocation domain for short-term reuse by one CPU. It is
volatile, reclaimable by other CPUs, and is not persistent ownership.

## Architecture

### Unified Physical Slot Allocator

Data and metadata pages currently consume slots from the same `free_ids`.
Therefore both classes must use one physical allocator. WAL is DRAM-resident in
the current implementation and uses `charge_wal_pages()` to consume PM capacity
without taking a slot. It is incorrect to introduce a data-only domain index
while metadata allocation can independently allocate the same slots through
the old global vector.

The replacement has the following structure:

```rust
struct NvSlotAllocator {
    domains: Box<[NvAllocationDomain]>,
    cpu_hints: PerCpu<NvDomainHint>,
    capacity: AtomicCapacityLedger,
    fallback_cursor: AtomicUsize,
}

struct NvAllocationDomain {
    slot_start: usize,
    slot_len: usize,
    state: SpinLock<NvDomainState>,
    free_pages: AtomicUsize,
    longest_free_range: AtomicUsize,
}

struct NvDomainState {
    free_bitmap: IdBitmap,
    free_ranges: FreeRangeIndex,
}
```

`FreeRangeIndex` maintains at least prefix-free, suffix-free, and longest-free
summaries over the domain bitmap. The existing EXT4 `FreeSpaceIndex` provides
the required algorithmic pattern, but the NVPage implementation must live in
the rNVPC module and use slot-number geometry rather than EXT4 block-group
geometry.

The atomic summaries are hints. Allocation always rechecks the bitmap/index
under the selected domain lock. A stale positive summary causes one failed
attempt. A negative summary may skip a domain only during the first selection
pass; it cannot justify `ENOSPC` or a fragmented-range result. Before final
failure, the allocator samples every domain's mutation generation, performs an
authoritative pass that locks each domain in turn and checks its free-range
index without consulting summaries, and samples all generations again. Any
change causes a retry/`EAGAIN`, not `ENOSPC`. This avoids a shared mutation
counter on the normal path. Summary and generation updates after
allocation/free use release ordering and readers use acquire ordering.

### Domain Count and Geometry

The pool must not create one small physical domain per CPU. With a 64 MiB data
limit, 16,384 data pages, 64 CPUs, and 128-page entries, 64 equal domains would
contain only 256 pages each. That geometry makes two large entries sufficient
to fragment or exhaust a CPU's home domain.

The initial policy is:

```text
minimum_domain_pages = 8 * max_redirect_extent_pages = 1024
candidate_domains = total_slot_pages / minimum_domain_pages
domain_count = largest power of two <= min(online_cpus, candidate_domains)
domain_count is at least 1
```

For a 16,384-page pool and 64 CPUs this selects 16 domains of approximately
1,024 pages. The last domain owns any remainder. Domain boundaries and count
are runtime geometry, not persistent format. Evaluation must include a
sensitivity test for 8, 16, and 32 domains before fixing the default.

Every physical slot has exactly one owner domain derived from its slot number.
A freed range always returns to that domain, regardless of which CPU performs
reclamation.

### Capacity Admission

Physical free-space ownership and class capacity policy are separate:

- the domain bitmap/index answers which slots are free;
- atomic capacity counters answer whether Data, Metadata, or WAL may consume
  more configured PM capacity;
- the data limit and high/low watermarks retain their current meaning.

An allocation first obtains a class reservation token using compare-exchange,
then searches an allocation domain. Failure to find physical space releases
the token. A successful free returns the physical range to its domain before
releasing the class charge to new allocators. This ordering may temporarily
under-report available capacity but cannot admit more allocations than there
are free slots.

For Data, admission reserves both `data_pages` and total free capacity. If the
second compare-exchange fails, it rolls back the data reservation and retries
or returns the existing pressure result. Metadata reserves total free capacity
before allocating a physical slot. WAL only reserves total capacity and updates
its class counter; it does not search an allocation domain. Statistics may read
counters independently and are therefore a momentary approximation; allocation
safety does not depend on a mutually consistent statistics snapshot.

The current repair workspace retains reusable metadata frames. Before a final
physical `ENOSPC`, Data or Metadata allocation drains an eligible repair frame
outside every domain and slot lock, returns it to its owner domain, and retries
admission and selection. This preserves the existing emergency-space behavior
without adding repair-workspace locking to the normal allocation critical
section.

The current global `next_alloc_seq` must not remain protected by the removed
allocation lock. While allocation generations remain part of legacy or
metadata identity, a request reserves one sequence-number interval using an
atomic compare-exchange/fetch-add with checked overflow. `RnvpcExtend` data may
stop using allocation generations only as part of the separately specified
ABA protocol change; this allocator refactoring must not silently remove an
identity check still used by recovery or a legacy mode.

High-water transition and backpressure counters become atomic. Wakeup policy
is driven by committed capacity changes, not by a domain's local free count.

### Allocation Policy

For a request of `N` contiguous slots:

1. Reserve `N` pages in the class capacity ledger.
2. Read the current CPU's hint. Try its last successful domain, then its home
   domain.
3. Skip a domain when `longest_free_range < N` or `free_pages < N`.
4. Lock one eligible domain, recheck its authoritative index, allocate one
   exact contiguous range, update summaries, and release the lock.
5. If the local choices fail, search other domains from a rotating fallback
   cursor. No attempt holds two domain locks simultaneously.
6. If the summary-guided pass fails, perform the authoritative no-summary pass
   described above. A concurrent domain generation change returns retry rather
   than a false capacity failure.
7. If no single domain can satisfy `N`, return a split allocation only when the
   caller permits multiple redirect entries. Use the minimum number of
   contiguous ranges and never combine non-contiguous slots or ranges from two
   allocation domains into one entry, even when their slot numbers are
   adjacent.
8. Initialize the allocated slot records after releasing the domain lock. The
   range is already unavailable to other allocators, so record initialization,
   NVDIMM persistence, data copy, and redirect publication do not extend the
   domain lock critical section.

If slot initialization fails before any persistent reference is published,
the initialized subset is cleared and the complete range returns to its owner
domain. If persistence may have made an uncertain allocation visible, the
range is quarantined and remains charged; it is not returned for reuse during
the current mount.

CPU hints use atomic fields because they affect only search order. A thread
migration may select another CPU's hint on the next call without changing
correctness.

### `RnvpcExtend` Redirect Publication

In `RnvpcMode::RnvpcExtend`, `RedirectDirectory` and the persistent redirect
extent entry are the logical-to-NV authority. `Rnvpc` itself does not own or
know the mounted filesystem mode, so low-level methods such as
`transition_data_page()` must not branch on a nonexistent pool mode. The
filesystem's `RnvpcExtend` call boundary instead uses explicit redirect APIs
that do not publish the duplicate legacy index. Generic/legacy APIs retain
their existing behavior until their users are migrated.

The implementation must audit every `PoolState::index` reader before removing
an update:

- `RnvpcExtend` reads and migration use the partitioned redirect directory;
- recovery reconstructs that directory from persistent redirect entries;
- rNVPC WAL or legacy modes retain their existing index path until separately
  redesigned.

The implementation introduces explicitly named `RnvpcExtend` redirect state
transition/publication/reclaim APIs. They update slot state and the partitioned
redirect directory without taking `Rnvpc::state`. The old generic transition
continues to update `PoolState::index` for callers that require it. The audit
must cover `transition_data_page()`, `publish_redirect_batch()`,
`assign_migration_targets()`, and `reclaim_nv_entry()` before their
`RnvpcExtend` callers move to the new API. Publication updates one redirect
partition per affected logical partition and persists the redirect entry
according to the existing soft-update order.

### Reclamation

Reclamation operates on a contiguous NV redirect extent entry:

1. Acquire the entry's semantic migration/reclaim lease and close new write and
   reuse admission for the complete runtime slot range.
2. Wait without a spinlock for active per-page readers to leave, then mark all
   page records reclaiming/persisting through ascending slot guards.
3. Release those guards and persistently remove or redirect the authoritative
   logical reference. Clear persistent slot descriptors in the required
   soft-update order.
4. Reacquire the entry and slot guards, compare the expected identity/lease,
   and retire the runtime page records. A mismatch retains the allocation and
   restarts from a new snapshot.
5. Return the physical range to its fixed owner domain. Allocation never builds
   one redirect entry across a domain boundary, so one entry normally requires
   one domain lock; recovery handles legacy cross-boundary ranges by splitting
   only the free operation.
6. Release the class capacity charge and wake capacity waiters after the range
   is visible in the domain index.

The implementation initially takes the owner domain lock directly. If remote
free contention is measured, a later optimization may enqueue ranges on a
per-domain lock-free return queue. Allocators drain that queue before searching
the domain. Delayed draining may delay reuse but cannot expose a range twice.

The wait queue should wake a bounded number of waiters proportional to the
number of newly available request units. Broadcasting to every blocked writer
after each reclaimed entry is prohibited because it creates avoidable lock
contention and failed allocation retries.

### Batched Range Lifecycle Operations

Allocation-domain locking removes cross-CPU allocator serialization, but a
128 KiB request can still acquire 32 independent object-slot locks for
initialization and free. This is not required for the first allocator phase,
because disjoint requests no longer contend on those slot locks, but it remains
per-page CPU overhead.

After domain allocation is stable, `RnvpcExtend` adds a range-lifecycle record
for each contiguous redirect entry. It coordinates whole-range allocation,
migration admission, and final release, and prevents a continuation slot from
being allocated independently. It does **not** replace the per-page `pins`,
`persisting`, `key`, version, and access-state fields used by
`pin_persistent_data_slot()` and concurrent readers/writers.

Batch initialization and final free acquire the affected slot guards in
ascending slot order through one range API, validate every page's access state,
and update all records before releasing the guards. Ordinary access continues
to lock or atomically pin only the addressed page; pages in the same redirect
entry are not serialized behind one head-slot lock. If profiling later shows
that acquiring the per-slot guards is itself significant, a separate design
may replace them with per-page atomic state arrays inside a partitioned object
table. This document does not assume such a representation.

### Optional CPU-local Free-range Cache

CPU-local caching is implemented only if measurements after allocation domains
still show material domain-lock wait time. It uses these constraints:

- each active CPU caches at most two contiguous ranges and at most 32 pages by
  default;
- all CPU caches together retain at most one eighth of currently usable pool
  capacity;
- a cached range remains associated with its owner domain;
- allocation pressure can request that other CPUs return cached ranges;
- an idle CPU returns its ranges; recovery starts with every cache empty;
- cached ranges are included in global free capacity but are not present in a
  domain index, so a fallback allocation must drain or transfer caches before
  concluding `ENOSPC`.

This cache is suitable for small random allocations. A 128-page request goes
directly to a domain so the cache does not fragment large sequential
allocations. The initial implementation must not prefetch 128 pages per CPU:
with 64 CPUs that could make half of a 64 MiB pool temporarily inaccessible to
other CPUs.

CPU-local hints use atomic fields and need no stable CPU residence for
correctness. A CPU-local cache is different: local allocation, remote drain,
and CPU-offline drain all acquire that cache's short-lived lock. The caller
disables preemption only while resolving the current CPU and popping/pushing a
range, then releases both the cache lock and preemption guard before any domain
lock, persistence, or I/O. Remote transfer removes a range from the source
cache under the same lock before publishing it to a domain or destination
cache; a cache generation detects a delayed transfer request. Consequently a
range cannot be allocated locally while it is being transferred.

## Locking and Ordering

Allocation is a sequence of short critical sections, not a nested lock chain:

```text
atomic class-capacity reservation
one allocation-domain lock: reserve a physical range, then unlock
ascending range/slot guards: initialize runtime records, then unlock
without spinlocks: persist objects and redirect records
short redirect-directory lock: publish the completed volatile mapping
```

The domain lock is released before range/slot initialization, persistence,
data copying, SSD submission, waiting, or redirect publication. No path holds
two domain locks at once.

Reclaim uses a semantic entry lease, not a spinlock guard held for the complete
operation. The order is:

```text
under the redirect/range lock: acquire the semantic lease, close admission,
  snapshot identity, then release the lock
without spinlocks: wait for existing readers
under ascending per-page access guards: mark reclaiming/persisting, then unlock
without spinlocks: persist reference/authority removal
under the redirect/range lock: compare identity and lease, commit volatile
  removal, then release the lock
under ascending per-page access guards: validate zero pins and retire records
one allocation-domain lock
atomic capacity release and waiter notification
```

No spinlock is held across NVDIMM persistence, SSD I/O, a device flush, or a
wait. A failed identity/lease comparison leaves the range allocated and retries
from a fresh snapshot. The exact implementation must preserve the existing
redirect lock order and must not acquire an inode publication guard under an
allocation-domain lock. Rollback code follows the reverse order and never
returns a slot whose persistent reference may still be reachable.

## Crash Consistency and Recovery

Failure model: the system may crash after any operation that has reached
NVDIMM or the block device. DRAM state, including domain indexes, CPU hints,
CPU-local caches, and locks, is lost. NVDIMM contents survive. This design does
not model NVDIMM media failure or SSD firmware behavior beyond the existing
flush and persistence-barrier semantics.

Persistent redirect entries and persistent object records remain authoritative;
allocation domains are derived state. Recovery:

1. validates persistent object and redirect records using the existing mode's
   recovery rules;
2. constructs every domain bitmap as free;
3. marks each reachable or conservatively quarantined persistent slot range
   allocated in its fixed owner domain;
4. rejects overlapping live ranges as `EUCLEAN` rather than choosing one;
5. rebuilds each `FreeRangeIndex` and publishes its atomic summaries;
6. recomputes Data and Metadata counters from persistent objects and initializes
   the WAL counter to zero because WAL is DRAM-only and is lost at a crash;
7. initializes CPU hints and optional caches empty.

A crash before redirect publication may leave an unreachable allocated range;
the recovery scanner may conservatively quarantine it or reclaim it only when
the existing persistent allocation protocol proves it unreachable. A crash
after reference removal but before free-index update cannot lose capacity
permanently because the volatile index is rebuilt.

## Implementation Phases

### Phase 0: Measurement

Add counters for wait time, hold time, calls, pages per call, full-vector sort
time, and failed contiguous searches on `allocation` and `state`. Keep these
counters disabled outside the diagnostic build if their overhead is material.

### Phase 1: Allocation Domains

Replace `free_ids` with the unified domain allocator and atomic capacity
ledger. Route Data and Metadata physical allocation, recovery, rollback, and
free paths through it. Route Data, Metadata, and WAL capacity accounting
through the atomic ledger. Do not add CPU-local page caches in this phase.

### Phase 2: `RnvpcExtend` Global-state Removal

Add explicit `RnvpcExtend` redirect APIs and remove their duplicate per-page
legacy index publication and reclaim updates after auditing all readers.
Preserve the old path for modes that still use it.

### Phase 3: Batched Range Lifecycle Operations

Batch `RnvpcExtend` contiguous-slot initialization and final reclaim through a
range-lifecycle API while retaining independent per-page access/pin state.
Verify every 4 KiB fallback and partial-failure path before replacing old
per-slot lifecycle loops.

### Phase 4: Optional CPU-local Cache

Enable the bounded CPU-local free-range cache only if Phase 1--3 profiles show
that domain lock wait remains a significant fraction of foreground write time.
Compare no-cache, 16-page, and 32-page limits and verify that large-entry
allocation success does not regress.

## Verification

### Correctness

- model-based randomized allocation/free tests compare every domain index with
  a reference bitmap;
- concurrent tests allocate from distinct home domains and prove no duplicate
  slot is returned;
- fallback tests exhaust one CPU's home domain and verify cross-domain search;
- contiguous tests cover 1, 32, and 128 pages and domain-boundary splitting;
- class tests interleave Data and Metadata physical allocation with WAL capacity
  charges under full-pool and data-limit pressure, and prove WAL never consumes
  a physical slot;
- rollback and injected persistence failures prove uncertain ranges are not
  reused;
- reclaim tests prove a range becomes allocatable only after persistent
  reference removal;
- recovery rebuilds domain indexes and capacity counters from mixed persistent
  object classes and rejects overlapping live ranges;
- rNVPC WAL and `RnvpcExtend` mode-isolation tests confirm that removing
  `RnvpcExtend` legacy-index updates does not change WAL lookup semantics.

### Performance

Use the existing 2 GiB fio workload and 128 MiB pool configuration, including
the 64 MiB data limit where applicable. Run at least three alternating rounds
for:

- 4 KiB random write, first-write and overwrite, jobs 1/2/4/8/16;
- 128 KiB sequential write, first-write and overwrite, jobs 1/2/4/8/16;
- the existing pipeline breakdown at 1 and 16 jobs.

Every point must complete fio, sync, unmount, and `e2fsck -fn`. Report median
bandwidth and the allocation/reclaim lock wait and hold distributions.

The initial acceptance criteria are:

- `RnvpcExtend` foreground allocation and reclaim contain no pool-wide allocation or
  page-index lock;
- full-vector free-ID sorting is absent from the write path;
- disjoint-domain concurrency tests show simultaneous progress;
- 128 KiB overwrite at 16 jobs improves by at least 25% over the pre-change
  median and retains at least 90% of the 1-job median;
- 1-job bandwidth does not regress by more than 5%;
- no correctness, recovery, or mode-isolation test regresses.

The throughput thresholds are evaluation gates, not correctness properties. If
lock wait falls but bandwidth does not improve, the next investigation should
profile range/slot initialization and migration coordination rather than add a
larger CPU-local cache without evidence.

## Rejected Alternatives

### Permanent Per-CPU Pools

Permanent ownership creates stranded capacity, makes thread migration and CPU
hotplug policy-visible, and fragments 128-page allocations. Recovery would
also need either persistent CPU ownership or a redistribution phase. Fixed
domains with CPU-local placement hints provide parallelism without these
properties.

### One Domain Per CPU

The current small pool would produce domains too close to the maximum entry
size. A few allocations would exhaust local contiguous space and force frequent
cross-CPU searching. Domain count is therefore derived from both CPU count and
minimum useful domain size.

### CPU-local Cache as the First Change

A cache would hide some acquisitions of the current global lock but would not
remove `free_ids` sorting, global `PoolState` updates, or batch reclaim
serialization. It can also retain a large fraction of a small pool. It is a
measured Phase 4 optimization after the authoritative allocator is partitioned.
