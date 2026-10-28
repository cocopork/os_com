# NVM-Authoritative EXT4 Allocation Metadata Cache

Date: 2026-10-28

Status: Approved for Native implementation.

## 1. Intent

rNVPCe migration must not synchronously read or write SSD allocation metadata.
The NVDIMM pool is authoritative for block allocation state while rNVPCe is
active. SSD block bitmaps, group descriptors, and the superblock free-block
counter are asynchronous writeback images, not migration dependencies.

This design removes `sync_rnvpce_allocation_metadata()` from the migration hot
path. A bounded NVM allocation-metadata cache stores the authoritative state of
active block groups. A separate metadata pager writes immutable snapshots to
SSD and changes a group's persistent directory state only after asynchronous
I/O completion and a device flush.

## 2. Scope

The first implementation covers allocation metadata modified when rTIO assigns
SSD data targets:

- the EXT4 block bitmap;
- the block-group descriptor fields derived from that bitmap, including the
  free-block count and checksum;
- the superblock free-block count, which recovery recomputes from group state.

It does not move inode bitmaps, inode-table blocks, directory blocks, or native
extent nodes into this cache. Those objects are not modified by rTIO's data
block allocation. Explicit sync/unmount may still wait for their existing
writeback paths.

The change applies to rNVPCe (`RnvpcExtend`) in nojournal mode. Native EXT4 and
the rNVPC WAL mode retain their existing allocation protocols.

## 3. Authority Model

NVM contains a persistent directory entry for every block group that rNVPCe
has admitted. The entry has one of these states:

- `CleanOnSsd(generation)`: the SSD bitmap and GDT image at `generation` are
  authoritative; no NVM bitmap slot is required.
- `ResidentClean(generation, slots)`: NVM contains the same image as SSD and is
  ready for allocation.
- `ResidentDirty(generation, slots)`: the NVM image is newer and is the only
  authoritative allocation state.
- `Loading(generation)`: an SSD image is being copied into NVM; it cannot serve
  allocation until NVM publication completes.

Only the persistent transitions `CleanOnSsd -> ResidentClean`,
`ResidentClean -> ResidentDirty`, and `ResidentDirty -> CleanOnSsd` affect
recovery authority. `Loading` and allocation-metadata I/O state are DRAM-only.
After a crash, an incomplete load remains `CleanOnSsd`; an incomplete writeback
remains resident and authoritative.

Each block group has two DRAM state bits. `DIRTY` means its allocation metadata
is newer than the SSD image. `WRITEBACK` means one allocation-metadata writeback
is in flight. Starting writeback atomically clears `DIRTY` and sets `WRITEBACK`
under the group allocation lock. Any allocation or free during I/O sets `DIRTY`
again. Completion may transfer authority only when `DIRTY=0`; otherwise it
clears `WRITEBACK`, keeps NVM authoritative, and queues another writeback.

Persistent `generation` is not a mutation counter. It selects a complete NVM
directory/chunk version and prevents an old completion from releasing a newer
object graph. There is no persistent or incrementing `mutation_epoch`.

Each block group also has at most one data-migration owner. A second data batch
or allocation-metadata writeback for the same group is deferred. Disjoint groups
may proceed independently. Foreground allocation remains allowed during
metadata writeback and only sets `DIRTY`; no group lock spans SSD I/O.

An eviction is valid only in this order:

```text
publish snapshot generation G in NVM and set WRITEBACK
  -> asynchronously submit bitmap and GDT after-images
  -> wait for both completions outside allocation locks
  -> issue and complete one device flush
  -> persist directory CleanOnSsd(G)
  -> release the NVM bitmap slots
```

If an allocation or free sets `DIRTY` while generation `G` is in flight, the
completion cannot evict it. The completed SSD write remains useful, but the NVM
directory stays resident and authoritative. A later writeback publishes the
current bitmap as a newer generation.

## 4. Persistent Representation

Each admitted group has a small `AllocationGroupDirectoryRecord`. It identifies
the filesystem, group number, generation, authority state, bitmap chunk
references, bitmap checksum, free-block count, and SSD-durable generation.
The directory record remains in NVM even when the bitmap is evicted.

A 4 KiB EXT4 bitmap cannot share one page with the persistent metadata header.
It is stored in two independently checksummed fixed-size
`AllocationBitmapChunkRecord` objects. The directory becomes reachable only
after both chunks are durable. Recovery rejects missing, duplicated, or
generation-mismatched chunks instead of falling back to stale SSD state.

The existing `AllocationRecord` continues to describe per-inode SSD capacity
credits. It is not an allocation bitmap and must not be used as one.

The persistent image version is bumped. Older pools do not contain a group
authority directory and are explicitly rejected for the new mode; there is no
implicit conversion.

## 5. Allocation Protocol

rTIO may allocate only from a `ResidentClean` or `ResidentDirty` group. The
group lock protects the DRAM buddy/bitmap and its matching NVM update. No block
device I/O occurs while that lock is held.

Allocating a range follows this soft-update order:

1. Set the allocated bits in the inactive NVM bitmap chunk copies and persist
   them.
2. Persist the directory generation, chunk selectors, free-block count, and
   `ResidentDirty` state.
3. Apply or confirm the matching DRAM allocator state.
4. Publish the SSD target into the NVPage descriptors.

A crash after step 1 but before step 2 leaves unreachable chunk copies, which
recovery reclaims. A crash after step 2 exposes allocated but possibly
unreferenced blocks; this is a leak, not double allocation. The existing
descriptor target publication and recovery scan either reuse or later reclaim
them.

Freeing uses the reverse dependency: remove every authoritative data/extent
reference first, then clear bitmap bits and publish the new group generation.
Free-block counters are derived values. Recovery recomputes them from the
selected bitmap and repairs the DRAM GDT/superblock views.

## 6. Cache Policy and Progress

The cache has explicit limits for resident directory entries and bitmap chunks.
The initial limit is derived from `meta_reserve_pages` after reserving the
existing rNVPCe extent/inode metadata budget; it is not allowed to consume data
pages.

Allocation prefers resident groups with sufficient free space. The pager
selects full groups first for eviction because rTIO cannot allocate from them.
It then considers clean least-recently-used groups. A dirty group requires the
writeback protocol before its chunks can be released.

When no suitable group is resident, `prepare_nv_targets` does not synchronously
read SSD metadata. It queues a load request, releases migration/allocation
locks, and ends the current migration round without assigning those targets.
The pager constructs the resident image from the current DRAM group image in a
normal boot; after recovery, the DRAM image is first loaded from the SSD
writeback and then overlaid by every NVM-authoritative resident group.

The pager maintains a low watermark of free metadata slots so ordinary
migration normally finds capacity without waiting. Permanent pager or device
failure sets the existing migration-failed state and wakes capacity waiters.

## 7. Fully Orderless Migration

`migrate_nv_round` performs no synchronous block-device operation:

1. claim NVPage descriptors;
2. allocate SSD targets by updating NVM-authoritative group metadata;
3. persist target identities in NVM;
4. submit data BIOs asynchronously;
5. seal the batch and return.

The batch becomes completion-ready after all data BIO callbacks finish. There
is no per-migration SSD bitmap/GDT BIO because allocation durability is already
provided by NVM. The completion worker issues one device flush for the data
batch, publishes the SSD baseline in NVM, and reclaims NVPages.

Metadata pager BIOs are also asynchronous. Data and metadata I/O may proceed
concurrently for different block groups, but one group has only one migration
or allocation-metadata writeback owner. Writeback state advances only after its
registered requests and a device flush complete. Sharing one physical flush is
an optional optimization; correctness does not depend on it.

The following APIs are forbidden from the rTIO flush call graph:

- `BlockDevice::read`, `read_val`, or `read_blocks`;
- `BlockDevice::write`, `write_val`, `write_bytes`, or `write_blocks`;
- `BlockDevice::sync`;
- `IoBatch::wait_all`.

Asynchronous `*_async` submission is allowed. The one data durability flush
remains in completion, not in flush/target preparation.

## 8. Recovery

Mount recovery first reconstructs the rNVPC pool and validates its filesystem
binding. Before enabling allocation or migration it then:

1. loads the SSD superblock, GDT, and block bitmaps as writeback images;
2. scans allocation-group directory records;
3. for every resident authoritative group, validates both selected bitmap
   chunks and overlays the SSD-derived DRAM bitmap;
4. recomputes each overlaid GDT free-block count and checksum state;
5. recomputes the superblock and runtime capacity counters;
6. protects pending migration targets and validates that every target is marked
   allocated by the selected authoritative bitmap;
7. starts the pager, then enables rTIO.

`CleanOnSsd(G)` permits use of the SSD image only after its directory transition
is durable. A resident entry with invalid chunks is `EUCLEAN`; recovery must not
silently accept the older SSD bitmap. Repeating recovery is idempotent.

## 9. Explicit Sync and Native Mountability

An explicit sync or clean unmount drains data migration, exports native
extents/inodes, writes back every dirty allocation group, and performs the
final device flush. Only then can the SSD EXT4 image be mounted without NVM.

After an unclean shutdown, native EXT4 and ordinary e2fsck do not understand the
NVM authority directory and must not mount or repair the device before rNVPCe
recovery. The existing research-mode binding/check remains mandatory.

## 10. Verification

Correctness tests cover crashes after every persistent transition, bitmap chunk
selector failure, stale writeback completion, cache-full eviction, repeated
recovery, pending-target protection, and I/O failure. A call-graph regression
test asserts that target preparation and flush contain zero synchronous block
I/O operations.

The end-to-end test uses a cache smaller than the number of touched groups so
that load and eviction both execute. After sync/unmount, `e2fsck -fn` must be
clean. Performance tests retain the established 2 GiB workload, 64 MiB dirty
limit, 32 MiB low watermark, CPU isolation, and 4 KiB random/128 KiB sequential
write points. Reports separate NVM metadata persistence, pager submission,
pager completion/flush, data submission, and completion publication.
