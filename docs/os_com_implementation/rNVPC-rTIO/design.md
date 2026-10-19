# AchillesFS Design

This is the authoritative mechanism design for AchillesFS. It describes the intended contract; implementation and validation status are maintained in [`../STATUS.md`](../STATUS.md), not inferred from this document.

## Mechanism Boundaries

| Component | Responsibility | Excluded responsibility |
| --- | --- | --- |
| rNVPC | Per-inode persistent WAL and foreground-checkpointed persistent PageCache B+tree | Real PM crash-consistency proof |
| rNVPCe | NV-resident data plane with soft-update consistency | WAL reuse or persistent runtime pointers |
| rTIO | Global NVPage candidate scheduling, migration-time SSD placement, and asynchronous completion | Persistent-object layout or inode-tree scanning for candidate discovery |

rTIO discovers work from a global dirty-NVPage queue rather than by walking files. A new NVPage owns persistent SSD capacity credit, not a predetermined SSD address; rTIO assigns concrete SSD blocks only when it forms a migration batch. A page promoted from an existing SSD mapping may reuse that already allocated block without consuming new credit.

## Persistent And Volatile Objects

Persistent objects are `NVPage` data, `NVBH` metadata, fixed-location rNVPCe inode records, one `SpaceReservationRecord` per inode, rNVPC WAL publication metadata, WAL records, persistent B+tree nodes, their identity/recovery descriptors, transient per-page migration targets/states, and the references required to reconstruct reachability. `NVPage` is the 4 KiB data unit; `NVBH` is the metadata-buffer unit and must not be conflated with a physical frame.

Volatile objects are the rNVPCe lookup B-tree, the runtime mirror of the rNVPC persistent B+tree, the per-NVPage reverse extent-slot index, the global dirty-NVPage queue, Rust pointers and handles, locks, reference and activity counts, the global sum of outstanding SSD reservations, free lists, queue nodes, and worker state. Persistent descriptors must never contain an address or control object that would be treated as valid after restart.

The lifecycle terms are deliberately distinct. A **committed** object has valid persistent contents but may still be unreachable staging or obsolete state. A **mapped** NVPage is the exact committed generation selected by the current inode extent tree. During recovery, the same property is described as **root-reachable**. **Reclaim** means durably clearing and returning an object to the free pool; it is permitted only after persistent references have been removed.

## Read Flow

rNVPC retains its DRAM PageCache mapping as part of the comparison design. rNVPCe does not use PageCache for reads or writes:

```text
rNVPCe lookup -> Nv:   read the NVPage directly
              -> miss: read the SSD block directly
```

An SSD read miss does not allocate or publish a clean DRAM page. The only volatile state is rebuildable lookup and concurrency metadata; it never stores file data.

## Write Flow

Capacity admission occurs before lower-level inode, page, or NVBH locks are held. The two variants deliberately use different persistence mechanisms.

For rNVPC, each write persists the new NVPage, appends and commits a persistent per-inode WAL record, and then publishes the DRAM PageCache mapping. When the fixed 1024-record WAL is full, the application thread synchronously replays every record, without same-page coalescing, into a fully persistent per-inode B-link B+tree. It advances the durable WAL head only after the tree and inode publication metadata are persistent. There is no background WAL checkpoint thread.

For rNVPCe, the fixed-location persistent inode is the recovery root. Buffered, synchronous, and direct writes all bypass PageCache. `copy_from_user` writes directly into a pinned NVPage range; no DRAM data page or bounce buffer is allocated.

If the logical page is already mapped to NVPool, the foreground writer excludes rTIO for that page, modifies the existing NVPage in place, persists exactly the copied range, and leaves the extent reference unchanged. A copy fault after a nonzero prefix produces a durable short write; a zero-byte copy returns the original error.

A hole or newly allocated page is initialized directly in its NVPage, including zeroing bytes not supplied by the user, before the extent publishes it. For a partial overwrite whose current source is on SSD, the block layer fills the new NVPage directly from SSD, `copy_from_user` overlays the requested range in that same NVPage, and the extent publishes it only after persistence. A full-page overwrite skips the SSD read. These allocations establish a new residency mapping; they are not retained old-data versions.

For each newly created logical block, the foreground write first adds one block to the inode's persistent `SpaceReservationRecord`. Admission checks subtract the global sum of outstanding reservations and the metadata reserve from EXT4 free space, so a successful write cannot later fail migration with `ENOSPC`. This reserves capacity only: neither the record nor the new NVPage contains a final SSD block address. Rewriting a block that already owns SSD space retains that allocation and does not reserve another block.

After data persistence, rNVPCe locally updates any required extent metadata and updates inode visibility fields such as size and block count last. Complex topology changes use only a lightweight per-inode `PendingExtentMutation` mark until the local repair is complete.

The current implementation also protects ordinary multi-field leaf rewrites with
a local repair image and pending mark. Its first optimization reuses a pool-wide,
bounded cache of cleared metadata frames (at most 64, also capped at one eighth of
the metadata reserve). Images are addressed directly, without ordinary metadata
index registration; pending marks retain their indexed recovery entry. Cleanup
uses the known mark reference, durably clears it before its images, and caches
only successfully cleared frames. Reuse assigns a fresh allocation sequence and
retains the existing descriptor commit protocol. Allocation pressure may drain
the cache. Thus this optimization removes object allocation/index churn, not
the persistent ordering required to repair torn multi-field updates. A compact
single-entry repair path remains a separate optimization.

rNVPCe does not promise that a write interrupted before returning is all-or-nothing. It promises that every persistence cut is structurally safe: an interrupted operation may leave a durable prefix and unreachable allocated objects, but never expose uninitialized bytes or a persistent reference to uninitialized data. A successful write returns only after all bytes in the returned range are durable.

## Writeback And Completion

rTIO uses a migration thread, a minimal BIO end-I/O callback, and a completion thread. Candidate discovery never traverses an inode extent tree. A transition to `NvDirty` inserts the exact page generation into a duplicate-suppressing global queue; stale queue entries are discarded by state and allocation-sequence validation.

The migration thread repeatedly performs:

```text
pop a bounded batch from global dirty-NVPage queue
  -> claim each exact page generation with a migration lease
  -> locate its leaf extent slot through the DRAM reverse index
  -> defer only candidates covered by an active split-range freeze
  -> acquire the local extent mutation gate and validate the forward reference
  -> allocate concrete SSD blocks, favoring one contiguous run for the batch
  -> persist each page descriptor's transient migration target/state
  -> mark pages InFlight and release extent mutation gates
  -> build and submit a merged BIO
  -> continue scanning without waiting for completion
```

No mutex, spin lock, or extent mutation gate is held across device I/O. The generation-checked migration lease pins the NVPage and excludes a foreground in-place overwrite; a writer encountering `InFlight` waits outside the inode lock and retries from the current mapping.

Each runtime object slot has its own lock and an inline 8-byte optional reverse locator.
Generation validation, pin admission, migration exclusion and reverse updates for
one NVPage remain joint operations under its slot lock. A separate allocation
lock protects free slots, capacity/watermarks (including WAL accounting), and
the next allocation sequence. Pool control state protects logical indexes,
legacy references and dirty-queue changes. Nested acquisition order is control
state -> allocation -> one slot; never acquire an outer lock while holding a
slot or nest slot locks.
The aggregate pin counter is atomic telemetry, not an admission or reclamation
condition. Persistence, user copying and waiting occur outside these spinlocks.
Migration validation reads the generation, owner and reverse locator in one
slot-lock acquisition, then validates the persistent SSD target outside that
lock under the existing migration lease; the leaf guard protects the eventual
forward-reference update. Slot reuse resets
the reverse locator; stale generations cannot query or clear a newer locator.

Publication ownership is inode-local: each native EXT4 inode caches an
`Arc<InodePublication>` containing its owner mutex and wait queue. Foreground
writes use this object; releasing its RAII guard wakes one
waiter for that inode. Acquiring/releasing cached publication ownership does not
take the pool control lock. A separately locked weak identity directory serves
inode initialization, recovery and standalone pool callers; it does not store
active ownership and removes expired entries. Guards and waiting callers retain
the object across inode eviction. The lock order remains native inode inner
lock before publication. Explicit metadata checkpoint takes inner first.

Leaf-level completion removes inode publication from data completion. A generation-checked
metadata-node guard excludes other node writers across read-modify-persist.
Foreground structural edits obtain these guards before taking live-node snapshots
and retain them through reverse repair. Completion holds one leaf guard, checks
its current reverse/forward edges and pending repair state, publishes SSD entries,
and drops it before touching another leaf or the separately guarded reservation
record. Completion locates pending repair marks through an independent volatile
directory keyed by `(fs_id,inode)` with64 inode-hashed mutex shards, rather than
through the pool-control metadata index. A shard is held only for reference lookup,
registration or generation-matched removal, never while persisting or pinning the
mark. Creation installs a conservative candidate before persistence; failed
admission restores the old entry, while ambiguous persistence failure keeps the
candidate. Successful durable clear removes only its exact generation. Recovery
lookup and runtime lookup validate committed state and generation before accepting
a cached reference; a stale directory entry cannot revive a cleared mark. Recovery
rebuilds this directory before pending-repair reconciliation. Hash collisions still
share a short lock; this is not strict per-inode locking.
Relocated reverse entries are resolved again. A failed pending mutation
cannot be overwritten by completion before recovery repairs its affected nodes.
Persistent metadata reads validate generation and committed state atomically with
pin admission, including after a wait. Pending-mark updates and metadata clearing
close reader admission and drain existing pins, preventing transient inspection
from failing foreground updates or attaching to already cleared repair frames.

The BIO end-I/O callback performs only interrupt-safe work: it records status in the owned completion item, appends that item to the preallocated bounded completion queue, and wakes the completion thread. It never modifies an extent, persists metadata, consumes reservation credit, reclaims an NVPage, or returns submission credit. In-flight credit is returned only after the completion worker drains the queue, which bounds callback-side occupancy.

Data completion does not install native SSD extents. The persistent fixed inode
and NV extent tree remain reachable after data reclamation; an SSD-selected NV
entry is sufficient to read the migrated block. Allocation ownership must reach
SSD before that entry is published. This implementation retains NV metadata;
it does not yet evict extent nodes or promise SSD-only recovery.

Explicit filesystem sync/unmount exports SSD-selected mappings from the NV tree
to native EXT4, under inode inner then publication exclusion. Background data
writeback and per-file fsync do not perform this export. NV-selected entries are
not exported as if their backing SSD data were current. The export is retryable
and does not reclaim NV metadata. A future metadata eviction path must additionally
persist its replacement and switch the persistent parent/root before reclaim.

Native-only fallocate, regular-file unlink and rename-overwrite are rejected on
rNVPCe mounts until they can mutate/reclaim the persistent NV graph as well.
This prevents native hole handling or inode reuse from discarding deferred SSD
ownership. Native EXT4 mounts retain their existing behavior.

The completion thread batches successful I/O durability boundaries and then performs, in process context:

```text
persist SSD allocation ownership and data (JBD2 or nojournal durability boundary)
  -> regroup completed pages by inode, in bounded 128-page windows
  -> resolve current reverse locators and group entries by leaf node
  -> acquire one leaf modification guard (no inode publication)
  -> validate page generations, targets, and current forward references
  -> publish all completed slots in each leaf with one persistent leaf update
  -> release the leaf guard; repeat for the remaining leaves
  -> mark each new block's credit consumed and refresh the inode ledger once
     under the reservation record's own modification guard
  -> clear the reverse entry and transient migration state
  -> mark the NVPage Available and reclaim it
  -> yield before the next bounded completion group
```

The transient migration target and phase live in the existing persistent NVPage descriptor; there is no separate migration log, anchor, or manifest. They let retry and recovery distinguish an unplaced page from a page that already owns a migration-time SSD allocation. An I/O failure keeps that durable target, returns the exact page generation to `NvDirty`, and retries without allocating another block. After SSD publication, a one-way descriptor phase ensures that the live EXT4 reservation counter is released exactly once. The per-inode persistent reservation record is a conservative ledger: failure to refresh it may temporarily over-reserve only in the persistent image, while runtime admission uses the exact atomic counter and recovery recomputes the ledger from the forward graph.

```text
NvDirty -> Claimed -> InFlight -> IoComplete -> extent published -> Available/reclaim
                         \-- I/O failure --------------------------> NvDirty/requeue
```

Submission and device completion are not reclaim boundaries. Until the SSD allocation and replacement extent reference are durable, the NV extent remains the selected mapping and the NVPage remains pinned.

After durable SSD publication, `EvictingDataPage::reclaim_after_ssd_publish`
consumes the migration lease. It validates the generation/lease under the slot
lock, clears the persistent descriptor outside locks, then takes allocation and
slot locks to clear reverse, end the pin, mark Free and return capacity together.
The second critical section uses debug assertions for the already validated
lease invariants; it does not repeat fallible runtime identity checks. Reverse
clearing remains unconditional in release builds.
It does not scan legacy references or acquire pool control, and has no separate
Synced-to-free call. Legacy callers retain the checked generic free interface.
Notifications occur after releasing locks; descriptor-clear failure never
returns the slot to the allocator.

Each mapped NVPage has one compact DRAM reverse locator identifying its current leaf extent slot. Forward extent publication and reverse-index installation occur under the same extent/page mutation exclusion, so an extent must never expose an NVPage while that page has an empty reverse entry. An empty reverse entry is therefore sufficient proof that no extent references the page; once completion and lifecycle conditions also permit reclamation, rTIO need not rescan the inode extent tree. The reverse index is an acceleration and safety structure for the running instance, not persistent metadata, and recovery rebuilds it from the persistent extent trees before migration or reclamation is enabled.

## Soft-Update Ordering

1. A persistent metadata reference never names a page whose data is not valid.
2. During a new residency mapping or relocation, the old mapping remains usable until persistent references select the initialized destination; an already resident NVPage may instead be overwritten in place.
3. A newly allocated resource is initialized before an extent or inode makes it reachable.
4. An extent or inode reference is removed before the referenced resource is released.
5. Inode visibility fields are persisted only after the data and extent state they expose.
6. Same-inode mutation is serialized; different inodes may proceed in parallel.
7. Capacity waits and I/O waits do not occur while holding foreground write locks.
8. Writeback keeps object-local dependency order but does not impose a global SSD order.
9. SSD capacity reservation precedes publication of a newly allocated NV mapping; concrete SSD allocation occurs only during rTIO migration.
10. SSD data and allocation metadata become durable before an extent replaces its NV reference; reservation consumption and NVPage reclamation occur afterwards.

The final extent or inode field update is the logical publication point; it is not represented by an additional persistent commit object. Slot reuse is validated independently with `(slot, allocation_seq)`.

Current scope excludes `mmap` and post-publication `resize`. In particular,
native EXT4 truncate cannot free an SSD block while a persistent rNVPCe extent
still names it; resize therefore returns `EOPNOTSUPP` until suffix deletion,
fixed-inode size publication, SSD reservation release, and NVPage reclamation
are implemented as one generation-checked soft-update operation.

## Pending Extent Mutation Protocol

`PendingExtentMutation` protects local extent-node rewrites and complex topology changes such as a child split, root growth, or a cascading parent split. It is a temporary operation-intent record, not a write-batch commit record or WAL. Same-inode mutation serialization permits one active mutation per inode. Ordinary writes to an already resident NVPage do not change an extent and therefore create no mark.

Before publishing the mark, the implementation persists temporary repair images for the existing node pages that may be rewritten in place. These images are reachable only through the mark, are never an alternative current extent tree, and are reclaimed after verification and mark removal. They let recovery restore a complete target page without decoding a potentially torn in-place image.

For a split at logical block `S`, consistency is determined by two persistent facts:

- `parent_references_new_child`: the parent contains a validated reference to the initialized right child at `S`;
- `old_child_repaired`: the old child contains only entries below `S` and no longer overlaps the right child.

The persistence sequence is:

```text
initialize and persist new child and repair images
  -> persist PendingExtentMutation(Prepared)
  -> publish and persist the parent reference
  -> persist PendingExtentMutation(ParentPublished)
  -> shrink and persist the old child range
  -> verify both facts and clear the mark
```

Publishing the parent before shrinking the old child intentionally permits a temporary overlap; it never permits an uncovered logical range. The phase narrows the expected recovery action, but recovery also inspects the actual topology because a crash may occur after the parent becomes durable and before the phase update becomes durable.

The temporary overlap also means that one moved NVPage may be described by both the old and new child while its compact reverse entry can name only one extent slot. Before publishing `Prepared`, the mutation therefore freezes exactly the moved logical interval `[S, old_end)`. rTIO must not migrate or reclaim NVPages mapped by that interval while the mark exists; pages below `S` and pages outside this split remain eligible. The reverse entries in the frozen interval remain non-empty throughout the operation, so they continue to prevent reclamation, but they are not treated as unique migration locators until repair completes.

After the old child has been durably shrunk, the mutator updates the affected reverse entries to their final new-child slots while retaining the interval freeze. It then verifies the forward/reverse relationship, clears `PendingExtentMutation`, and releases the freeze. The mutation and rTIO paths use the same range-aware exclusion, so no migration can observe the interval between final forward repair and reverse-index correction. No additional persistent freeze bit is required: during normal execution the live mutation owns the freeze, and after a crash recovery derives the interval from `PendingExtentMutation` before rTIO starts.

Recovery applies this table before exposing the inode:

| Durable observation | Recovery action |
| --- | --- |
| `Prepared`, parent does not reference the new child | Restore the parent from its repair image, advance the phase, and roll forward. |
| `Prepared`, parent references the new child | Treat publication as completed, advance the phase, and roll forward. |
| `ParentPublished`, parent references the new child, old child still overlaps or is torn | Restore the repaired old child image. |
| Parent references the new child and old child is already repaired | Verify the topology and clear the mark. |
| `ParentPublished`, but parent does not reference the new child | Report corruption; this contradicts the required persistence order. |

Every repair step is idempotent. Root growth follows the same rule, with the fixed inode root acting as the parent publication field. The mark is cleared last and its repair images are then reclaimed, so the absence of a valid mark means no local topology repair remains. Every recorded object reference is accepted only when `(slot, allocation_seq)`, owner inode, level, and split boundary all validate.

## Recovery Model

Crash-consistency simulation is deferred while performance work proceeds.
Normal kernels contain only the working DRAM image; durable-image allocation
and working-to-durable copy code are compiled only for ktest fixtures. Normal
backend allocation rejects `rnvpc_sim_mode=model` explicitly. Performance runs
use fast or fast-no-persist and do not provide crash-durability evidence. The
recovery protocol below remains the design target for later validation.
Ordinary fast persist/persist_control retain validation, compiler fences and
telemetry but no longer acquire the image-copy io_lock. Ordinary data and
descriptor byte accesses also have no backend-wide lock. Object lifetime,
overlapping mutation and migration exclusion are the caller's responsibility
through pins, mutation leases and inode publication. Bulk byte reads are not
atomic snapshots. Only explicit legacy model fixtures retain image locking;
fast/no-persist ktests exercise the ordinary unlocked path.

Recovery scans persistent NVPage/NVBH descriptors plus rNVPC WAL publication metadata, WAL frames, and B+tree nodes. For rNVPC it selects each inode's newest valid publication state, restores locally double-imaged B-link nodes, replays every committed WAL record newer than `checkpointed_lsn`, and then rebuilds the DRAM PageCache index.

For rNVPCe, recovery starts from each fixed persistent inode, reconstructs the frozen interval of every `PendingExtentMutation`, and rolls each mutation forward from its repair images before exposing the inode. It then traverses `inode -> extent nodes -> NVPages` and rebuilds the volatile residency, reverse extent-slot, and global dirty-NVPage indexes; ES-tree and extent-cache state remain empty for bounded lazy reconstruction. It scans per-inode `SpaceReservationRecord`s and reconciles the exact outstanding total from the recovered forward graph. If an interrupted migration is still selected by an NV extent, its descriptor-local SSD target remains owned and the page is requeued for retry; if the extent already selects SSD, the old NV descriptor is unreachable and reclaimable. The reverse index must be complete and forward/reverse consistency must validate before rTIO migration or runtime reclamation is enabled. A persistent reference is accepted only when both its slot and allocation sequence match. A committed frame has valid persistent contents but is not necessarily mapped or reachable. Committed frames not reachable from a fixed inode root are leaked staging or obsolete objects and may be reclaimed after the service-critical indexes are available. Recovery discards prior DRAM pages, locks, pointers, handles, queues, and worker state.

## Invariants

Completion batches retain physical-view BIO submission, then regroup successful,
SSD-durable pages by inode and current reverse-located leaf. Publication ownership
is bounded to 128 pages per inode group. Resolve reverse locators only after
acquiring that ownership, validate all entries of each leaf before modifying it,
and persist one combined leaf image. Failed leaf groups retain their NVPages for
retry; successful groups can reclaim independently. Each page retains its
idempotent migration-credit phase, while the inode reservation ledger is refreshed
once for the group's newly consumed credits. Incomplete pages in the same leaf
remain NV-backed; completion never waits to collect an entire extent. This reuses
the existing in-place metadata persistence protocol and does not introduce a new
whole-leaf crash-atomicity guarantee.

- Persistent data contains no Rust pointer, lock, or runtime reference count.
- DRAM is never the sole recovery copy.
- NV data validity precedes persistent reference publication.
- rNVPCe file data never passes through PageCache or a DRAM bounce buffer.
- Existing NVPages are overwritten in place; newly allocated NVPages are fully initialized before publication.
- Foreground mutation of a mapped NVPage excludes same-page rTIO writeback.
- Every published NVPage extent has a non-empty DRAM reverse locator while the filesystem is running.
- Candidate discovery uses the global dirty-NVPage queue and never walks inode extent trees.
- New NV mappings reserve SSD capacity without choosing SSD positions; only rTIO assigns concrete blocks.
- End-I/O callbacks never mutate persistent filesystem metadata or reclaim NVPages.
- No ordinary lock is held across asynchronous BIO execution.
- A split freezes only its moved interval; rTIO cannot migrate or reclaim that interval until forward repair and reverse-index correction both complete.
- Recovery rebuilds and validates reverse locators before enabling rTIO or runtime reclamation.
- Reference removal precedes old-object reclamation.
- The mapped rNVPCe data set starts at fixed persistent inode roots and follows the persistent extent topology.
- A crash before `write()` returns may expose a safe prefix, while a successful return means the complete write is durable.
- One inode has at most one active writeback owner.
- Foreground B+tree checkpoint permits rNVPC WAL retirement; SSD completion permits NVPage reclamation.
- Completion failure preserves dirty data and retry state.
- rNVPC WAL accounting and rNVPCe in-flight accounting remain separate.
- Cross-inode physical reordering is not a correctness dependency.

## Implementation Boundary

The approved rNVPC foreground-checkpoint design is specified in [`rnvpc-foreground-checkpoint-design.md`](rnvpc-foreground-checkpoint-design.md). The accepted rNVPCe fixed-inode design and migration tasks are specified in [`rnvpce-soft-update-implementation-plan.md`](rnvpce-soft-update-implementation-plan.md). The active source implements the fixed-inode, count-reservation, global-dirty-queue, reverse-index, and deferred-completion model; the former inode-scanning `rtio.rs` production path has been removed. See [`../STATUS.md`](../STATUS.md) for the exact validation boundary.


### rNVPCe runtime journal policy (2026-10-18)

rNVPCe mounts run without JBD2, including on a volume retaining a clean internal
journal. Pending filesystem/journal recovery is rejected before writes or worker
startup; native EXT4 must recover the journal first. The on-disk journal feature
is retained. SSD allocation/data durability boundaries remain mandatory.
Nojournal native extent export coalesces node after-images in64-block chunks,
then writes the native inode. This does not implement NVPage target/credit/reclaim
batch transactions, nor establish full namespace/bitmap power-loss consistency.
See `../os_com_refactor_plan/009-rnvpce-nojournal/README.md` and the independent
`../os_com_refactor_plan/010-nvpage-batch-transactions/README.md`.
