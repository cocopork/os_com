# Scalable rNVPCe SSD Allocation Design

Date: 2026-10-30

Status: Approved in chat for Native implementation.

## Intent

rNVPCe first-write allocation must scale across CPUs without scanning every
EXT4 block group or serializing disjoint groups behind one migration-owner
lock. The allocator must preserve NVM-authoritative allocation state and the
soft-update rule that an SSD target is never published before its block is
durably reserved.

The existing EXT4 allocator already has the required group-local foundations:
`BlockGroup::allocation_gate`, a group metadata lock, and `FreeSpaceIndex`, a
bounded segment-tree index over the cached bitmap. This work reuses those
structures. It does not add inode or locality-group preallocation.

## Goals

- Normal first-write allocation does not enumerate or sort all block groups.
- Disjoint block groups allocate concurrently; the only normal-path allocator
  serialization is the selected group's existing locks.
- A CPU normally searches only its own stripe of block groups and remembers
  its last successful group.
- Group-local allocation uses `FreeSpaceIndex`; it never linearly scans the
  block bitmap.
- A resident NVM allocation bitmap is updated in place at aligned bitmap-word
  granularity instead of replacing two bitmap chunks and one directory after
  every allocation.
- Allocation-metadata writeback snapshots one group briefly, performs SSD I/O
  without an allocation lock, and hands authority to SSD only if that group
  was not redirtied.
- Recovery derives the group free count and in-memory free-space index from the
  selected authoritative bitmap.

## Non-goals

- inode preallocation or Linux locality-group preallocation;
- per-CPU ownership of permanently reserved disk extents;
- changing native EXT4 journal semantics;
- sharing one bitmap or allocation lock across multiple block groups;
- making NVDIMM media-error recovery stronger than the existing persistent
  object identity checks.

## Existing Problems

`reserve_nv_allocation_groups()` currently reads every group's free count,
sorts the complete vector, and performs selection while holding
`nv_migration_groups`. A 4 KiB first write then snapshots the complete 4 KiB
group bitmap and `AllocationMetadataCache::publish_dirty_bitmap()` replaces
both chunk objects and the directory record. Thus one data page pays global
selection plus full-image persistent metadata publication.

The global owner set also combines unrelated concerns: excluding a data
migration from a group's allocation-metadata handoff, reserving groups for
foreground allocation, and keeping an old completion from releasing a newer
owner. These are group-local states and should not serialize other groups.

## Architecture

### Per-CPU Group Selection

`AllocationGroupSelector` belongs to one mounted `Ext4` instance. It contains
one atomic cursor per CPU and one atomic summary per block group. A summary
contains the last published free-block count and longest free run. It is a
hint: `BlockGroup::alloc_blocks()` remains the authority and rechecks under the
group lock.

Let `P = min(online CPUs, G)`. For CPU `c`, its selector lane is `c mod P`
and the home stripe is:

```text
(c mod P), (c mod P) + P, (c mod P) + 2P, ... < G
```

Selection order is:

1. the caller's exact goal group when the previous logical block supplied a
   physical-contiguity goal;
2. the CPU's remembered successful group;
3. at most one pass over that CPU's home stripe, starting at its cursor;
4. a steal pass over atomic summaries from a rotating global start point.

The exact-goal and last-success groups are de-duplicated from later passes. The
steal pass is a near-full-disk fallback, not the normal path. It reads only
summaries and never takes another group's metadata lock merely to inspect its
bitmap. `ENOSPC` is returned only after the capacity ledger denies the request
or the fallback proves that no summary can satisfy it. Lock conflicts return
`EBUSY`/retry, not `ENOSPC`.

CPU identity is a placement hint only. Thread migration changes the preferred
stripe but cannot affect correctness or ownership.

### Group-local Ownership

`AllocationWritebackState` gains a data-migration owner generation. The state
continues to live behind `BlockGroup::allocation_gate` and represents:

```text
dirty
writeback_in_progress
data_migration_owner: Option<u64>
next_owner_generation
```

Only one data migration or allocation-metadata authority handoff may own a
group at a time. Foreground allocation takes the gate only while modifying the
group, rejects a group with either owner, marks it dirty, and releases it before
any wait or device I/O. Foreground allocation does not retain a lease: its NVM
allocation words are durable before the gate is released, and its target is
published only afterward. A pager snapshot in between is safe because it can
only make that reservation durable on SSD before target publication.

An rTIO batch acquires a data-migration lease before target allocation and
retains it through data completion, durability flush, and completion-state
publication. The lease is released before page reclamation is offered to other
threads. Disjoint groups therefore proceed independently. An RAII
`AllocationGroupLease`
releases exactly the generation it acquired, so an old completion cannot clear
a newer owner.

No operation holds two group allocation gates simultaneously. Multi-group
batches acquire, mutate, and release groups in ascending order one at a time;
the batch retains only generation-tagged leases.

### Group-local Free-space Index

`FreeSpaceIndex` remains the DRAM search authority. It gains a
`longest_free_run() -> u32` query used to publish the atomic group summary. Allocation
and free already refresh affected word ranges. Recovery and NVM overlay rebuild
the index from the authoritative bitmap.

The summary is updated under the group metadata lock after bitmap/index
mutation and published with release ordering. Readers use acquire ordering.
False-negative summaries only defer a group until its next update; false
positives are rejected by the authoritative allocation attempt.

### Incremental NVM Bitmap

The persistent directory continues to give each group stable chunk object
references. Once resident, those chunk identities do not change for each
allocation. Bitmap data is an array of aligned native-endian `u64` words.

Allocation uses this order under the group allocation gate:

```text
set the selected bits in the DRAM bitmap and FreeSpaceIndex
  -> persist every touched NVM bitmap word/range
  -> one persistence fence
  -> mark the group ResidentDirty/dirty in DRAM
  -> publish the SSD target into the write transaction
```

Freeing uses the reverse dependency:

```text
remove every authoritative data/extent/redirect reference
  -> persist that removal
  -> clear the released NVM bitmap words
  -> one persistence fence
  -> release the blocks in the DRAM bitmap/index
```

Aligned 64-bit word stores are the allocation-state commit units. A crash in a
multiword allocation may reserve only a prefix/subset, which is an unreachable
space leak, never double allocation or a dangling authoritative reference. A
crash during free may leave allocated bits after references are gone, also a
leak. Recovery may reclaim such leaks later but must never infer a free block
from a stale derived count.

The mutable resident chunk format uses `checksum == 0` as an explicit format
marker. In that format `AllocationBitmapChunkRecord::validate()` validates the
immutable header, object identity, group, chunk index, length, and geometry but
does not compare a payload checksum that would become stale after the first
word update. Legacy nonzero-checksum chunks retain full checksum validation and
are converted to mutable chunks when admitted. This does not weaken the stated
media-error model: stronger NVDIMM payload corruption detection is a non-goal.

While resident, directory `free_blocks` and whole-bitmap checksum are derived
snapshot fields, not per-allocation commit points and are ignored for authority
selection in `ResidentDirty`. Recovery validates object identity and bitmap
geometry, then recomputes free count, checksum, and `FreeSpaceIndex` from bitmap
words. A writeback snapshot computes exact derived fields before issuing SSD
bitmap/GDT writes. A clean-on-SSD directory records that exact snapshot; its
resident chunks are then reclaimable.

### Live Persistence Failure

The allocator distinguishes failure before any NVM word can have changed from
failure during or after range persistence. Before-change failure rolls the DRAM
allocation back normally. Once persistence may have begun, the entire requested
range is quarantined: its DRAM bits remain allocated, `FreeSpaceIndex` excludes
it, its blocks remain consumed in `BlockCapacity` and the superblock free count,
and no SSD target is published. The operation returns `EIO` and marks the group
for recovery/fsck; it never clears uncertain durable bits or refunds them for
reuse. A free-path persistence failure likewise retains the allocation bits and
capacity charge. Thus an online error can leak space but cannot double allocate.

### Capacity Reservation

The implementation reuses the existing `BlockCapacity` contract. One request
claims capacity before group selection, releases the capacity lock, and consumes
the reservation as each range becomes allocated. A range that reaches the
possibly-persisted/quarantined state is consumed, not refunded. Only a range
known not to have reached NVM may be rolled back and returned. Partial allocation
keeps the unconsumed remainder reserved while trying later candidates and drops
it on final failure. `ENOSPC` comes from failed admission or an exhaustive
summary/authoritative attempt; busy groups produce retry rather than capacity
failure.

### Allocation-metadata Writeback

The pager protocol for one group is:

1. Acquire the group gate, reject an active data-migration owner, set
   `writeback`, clear `dirty`, and copy a stable bitmap/GDT after-image.
2. Release the gate and submit SSD writes.
3. Wait for completion and issue the durability flush without the gate.
4. Reacquire the gate. If `dirty` is still clear and the writeback generation
   matches, persist `CleanOnSsd` and release resident bitmap objects.
5. If allocation redirtied the group, clear `writeback`, retain NVM authority,
   and enqueue another writeback.

An allocation may proceed during SSD I/O after the snapshot; it updates the
resident NVM bitmap first and sets `dirty`. No SSD I/O, wait queue sleep, or
device flush occurs under a group lock.

## Lock Order

The permitted order is:

```text
capacity ledger
  (released before selection)
group allocation gate
group metadata write lock
NVM object-slot mutation lock
```

The superblock publication lock is acquired only after all group locks are
released. The pager never holds a group lock during SSD I/O. The inode
publication guard must not be acquired while holding a group allocation gate.

## Recovery

For every resident allocation group, recovery:

1. validates directory and chunk object identities and geometry;
2. reads the stable bitmap words;
3. overlays the SSD-derived DRAM bitmap;
4. recomputes free count and checksum;
5. rebuilds `FreeSpaceIndex` and publishes the group summary;
6. recomputes the filesystem capacity ledger;
7. verifies that every published migration target is allocated.

An incomplete allocation can leak blocks but cannot make an allocated block
available. An invalid or missing resident bitmap object remains `EUCLEAN`; it
must not silently fall back to an older SSD bitmap.

A published target whose bit is clear in the selected authoritative bitmap is
also `EUCLEAN` and fails recovery. Recovery must not set that bit as a repair:
the clear bit may already have allowed the block to be assigned elsewhere, so
blind repair could preserve two owners.

## Verification

- deterministic selector tests prove CPUs start in disjoint stripes, retain a
  successful group, steal after local exhaustion, and never report `ENOSPC`
  solely because a candidate group is busy;
- concurrent tests prove allocations in two groups progress independently and
  stale lease completion cannot release a newer owner;
- model-based `FreeSpaceIndex` tests continue to compare against the bitmap;
- crash-cut tests cover each incremental allocation/free persist boundary;
- injected live persist failures prove uncertain ranges remain quarantined and
  charged while no target becomes visible;
- recovery reconstructs counts and summaries from bitmap words;
- real-device tests run 4 KiB sequential first-write at 1/2/4/8/16 jobs and
  128 KiB sequential first-write at 1/16 jobs, each with 2 GiB total and the
  existing 64 MiB data limit;
- every performance point must complete fio, sync, unmount, and `e2fsck -fn`.
