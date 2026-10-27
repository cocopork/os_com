# Per-Page Descriptor Redirection for rNVPCe

Date: 2026-10-27

Status: Design approved for planning; not implemented.

## 1. Intent

The rNVPCe foreground write path must not split or rewrite the persistent extent
tree merely because one page temporarily moves from SSD to NVM. SSD extents and
NVM residency describe different facts and must be represented separately:

- the NVDIMM extent tree records the confirmed SSD baseline mapping, including
  hole and unwritten semantics;
- each committed NVPage descriptor records a newer per-page override;
- a rebuildable DRAM redirect index coalesces adjacent overrides into
  contiguous page ranges for lookup, writeback selection, and batched reclaim.

This replaces the current mixed `ExtentRecordEntry` model in which the entry
target says either SSD or NVM. It also removes the requirement that rTIO claim
and validate every member of an extent before submitting migration I/O.

The design applies to rNVPCe. It does not change the rNVPC WAL protocol.

## 2. Problem

The current mixed extent representation has two coupled costs.

First, one extent is treated as having one location even though residency is
per page. A 128 KiB logical extent may contain dirty NVM pages, clean SSD pages,
and pages currently in writeback. Claiming the extent therefore reads its leaf
and validates every page descriptor and reverse edge before migration.

Second, a partial overwrite changes the extent topology. Updating page 4 in an
SSD extent `[0, 8)` produces at least:

```text
SSD [0, 4) | NV [4, 5) | SSD [5, 8)
```

That operation requires entry insertion, range changes, possible node split,
reverse-index repair, and several persistence ordering points. These operations
describe temporary cache residency rather than a real change to the file's SSD
layout.

## 3. Chosen Model

```text
                         persistent

  inode -> confirmed NVM baseline tree ---------> SSD block mapping
                  ^
                  | used only when no active redirect exists
                  |
  NV slot -> selected NVPage descriptor
             { fs, inode, logical page,
               logical version, allocation generation,
               state, optional SSD migration target }
                  |
                  +-----------------------------> NV override authority

                         volatile

  descriptor scan/publication -> PageKey map -> RedirectRange index
                                              -> dirty-page-range queue
```

For a logical page, an active selected NVPage descriptor always overrides the
confirmed NVM baseline tree. When no active descriptor exists, lookup falls
back to that baseline. The DRAM index is never authoritative and may be
discarded and rebuilt by scanning selected descriptors.

The on-device EXT4 extent tree is not independently authoritative during an
orderless migration. Its metadata may reach SSD before its data or before NVM
records confirmation. Recovery trusts the confirmed NVM baseline plus active
NVPage descriptors; an SSD extent visible only in prematurely persisted EXT4
metadata is ignored, repaired, or treated as leaked allocation. It does not
make unconfirmed SSD data visible.

There is no persistent per-extent redirect table. The selected descriptor is
the persistent redirect record, and its slot identifies the NVPage. This uses
the existing self-describing layout rather than introducing a second persistent
object whose publication and reclamation would need another protocol.

## 4. Persistent Identity and Authority

Each data descriptor has two distinct generations:

- `allocation_seq` identifies one use of a physical NV slot and prevents slot
  reuse ABA;
- `logical_version` orders successive values of one
  `(fs_id, inode, logical_page)` key.

The existing layout already stores these concepts as `reserved` and
`recovery_id`; implementation must rename or wrap them so their roles cannot be
confused. A format change is required only if the final state encoding cannot
be expressed by the existing fields and flags.

For each `PageKey`, the active redirect is the valid selected descriptor with
the greatest `logical_version`. Equal-version descriptors for the same key are
corruption. Version overflow rejects the write. A descriptor from a reused slot
is never accepted through an older `allocation_seq`.

Publishing a new value follows this order:

1. Allocate one or more NV slots and, under the affected PageKey shard locks,
   install pending-publication tokens that reserve monotonically newer logical
   versions. Readers continue to use the current committed versions. Another
   writer must observe the reservation and choose a still newer version or wait;
   two writers cannot both prepare version `N+1`.
2. Write and persist all NVPage data.
3. Write the inactive descriptor copies and persist them.
4. Publish their aligned selector words; batch selector persistence under one
   final fence.
5. Replace the pending tokens with the new committed range in the DRAM redirect
   index.
6. Retire older descriptors after readers, writers, and migration requests that
   reference their exact allocation generations have left.

A crash before step 4 retains the old redirect or SSD baseline. A crash after
step 4 selects the new descriptor. The old descriptor may remain allocated, but
recovery chooses the higher logical version and reclaims the unreachable old
generation only after rebuilding authority.

If persistence fails before selector publication, the writer removes its
pending token and releases the uncommitted slot. If selector durability becomes
uncertain, the pool enters its existing recovery-required state rather than
allowing another version to reuse either slot.

Every valid selected data descriptor remains the authoritative redirect,
whether its state is `NvDirty`, `NvWriteback`, or `NvReclaimable`, and whether
its migration-target or credit-released flags are set. Only durably clearing
its selector removes redirect authority. A migration target or a completed SSD
BIO never becomes authoritative by itself.

## 5. DRAM Redirect Index

The runtime maintains two related, rebuildable indexes.

`PageRedirectMap` maps `PageKey` to an exact descriptor handle:

```text
{ nv_slot, allocation_seq, logical_version, state, ssd_target }
```

`RedirectRangeIndex` records a contiguous page range (continuous logical page
numbers mapped to continuous NV slots) when the pages also have compatible
lifecycle states. Each `RedirectRange` retains the exact per-page allocation
generations; it must not infer them arithmetically. Its identity and mutation
epoch exist only in DRAM. It also captures the current runtime baseline-range
generation for the covered logical range.

The indexes are sharded by `(fs_id, inode, logical_page)` range. Foreground
writes update only the affected shards. A partial overwrite splits a DRAM
contiguous page range, not a persistent extent. Adjacent compatible ranges may
be merged without NVM writes.

Recovery scans descriptors, selects the highest logical version per PageKey,
sorts selected data descriptors by owner and logical page, and coalesces them
into contiguous page ranges. The existing recovery path must be changed:
insertion in physical slot order without comparing data versions is not
sufficient.

The runtime also maintains a rebuildable range-generation map for confirmed SSD
baseline mappings. Every true baseline mutation obtains range coordination and
advances the intersecting generation. It is a concurrency detector, not
persistent authority; recovery rebuilds it from the confirmed NVM baseline.

## 6. Read and Write Paths

### Read

Lookup checks `PageRedirectMap` first. A hit pins the exact
`(slot, allocation_seq, logical_version)` and reads the NVPage. A miss consults
the SSD extent tree and applies its written, unwritten, or hole semantics.

The index hit must be rechecked while acquiring the page pin. This is an
in-memory identity check; it does not read the persistent descriptor or extent
leaf on every lookup.

### Foreground write

A write allocates a contiguous NVPage range when possible, prepares page data,
and publishes all descriptors as one bounded batch. It does not modify the
extent tree when the SSD baseline mapping already exists.

If a page's old generation is in writeback, the writer uses copy-on-write at
NVPage granularity: it publishes a new descriptor version and leaves the pinned
old generation for completion. Thus foreground write does not wait for an
extent-wide migration lease.

Hole writes, file extension, unwritten conversion, truncate, and hole punch are
not ordinary residency changes. They also update persistent file/SSD allocation
state using the structural protocol described in section 9.

## 7. rTIO Candidate Selection

Candidates come from the dirty-page-range queue, not from the extent tree and
not by scanning physical NVM addresses without logical grouping. A dirty page
range is a `RedirectRange` whose pages require SSD writeback.

For each selected dirty page range, rTIO performs one sharded in-memory claim:

1. Snapshot the range identity and its exact descriptor handles.
2. Confirm that the snapshot is still current in `PageRedirectMap`.
3. Pin the referenced allocation generations and mark them writeback.
4. Release the index shard before allocation, metadata work, or device I/O.

The claim does not read an extent leaf, query an NVPage-to-extent reverse
locator, or reread each persistent descriptor. Descriptor state changes and SSD
target assignments are still persisted in bounded batches because they are
needed for crash recovery, but validation uses the already-owned runtime
objects.

An in-memory contiguous page range is an optimization, not an all-or-nothing
persistent extent. The batch may be shortened at a budget boundary.
Fragmentation or one stale page splits the volatile candidate into valid
subranges rather than rejecting all other pages.

## 8. Orderless Migration and Completion

Data BIOs and required EXT4 metadata BIOs are submitted without ordering them
against each other. Once every request in the migration batch completes, one
device flush establishes SSD durability.

Completion then handles two cases.

### Existing SSD baseline

When the confirmed baseline already maps each page to the migration target,
completion normally does not touch the extent tree. Candidate preparation
captures the relevant baseline-range generation. Completion first compares it
with the current generation:

- an equal generation proves that no structural operation changed the mapping,
  so the common path performs no leaf read;
- a changed generation enters a slow path that resolves the current baseline
  and retains or requeues redirects whose targets no longer match.

Completion then compares the current DRAM redirect versions with the submitted
page range:

- matching old generations may have their descriptor selectors cleared in one
  bounded batch;
- a page replaced by a newer foreground write retains the new redirect while
  the old submitted descriptor is retired;
- adjacent matching pages are reclaimed and returned as contiguous slot ranges.

The SSD now contains the submitted version. If a newer NVM redirect exists, it
still overrides that SSD content and will be migrated later.

### New or changed SSD baseline

For a hole, delayed allocation, unwritten conversion, or relocated target:

1. Persist allocation ownership in the NVPage descriptors/allocation ledger
   before submitting SSD metadata that can expose those blocks.
2. SSD data and metadata requests complete and the batch device flush succeeds.
3. Publish the confirmed NVM baseline using the structural extent protocol and
   advance its runtime range generation.
4. Persistently release the allocation credit. Recovery can reconstruct a
   credit whose release record lags the confirmed baseline.
5. Clear matching per-page redirect selectors in bounded batches.
6. Reclaim their slots only after selector durability and reader/pin quiescence.

Publishing the baseline before releasing credit and clearing redirects prevents
a page from becoming unreachable or an SSD allocation from losing its owner. A
crash before baseline publication retains the NVM redirect and retries
migration. A crash after baseline publication but before redirect clearing
still reads the NVM redirect; recovery may safely finish credit and redirect
cleanup.

Overlapping writes to the same SSD target are serialized by PageKey migration
ownership. A newer descriptor may be published while an older I/O is in flight,
but its SSD write is not submitted until the older target write and flush have
completed. This prevents an older orderless BIO from overwriting a newer value.

## 9. Remaining Role of FAST/FAIR

FAST/FAIR remains necessary for genuine persistent extent-tree changes:

- allocating SSD blocks for holes or delayed allocation;
- inserting, deleting, splitting, and merging SSD extents;
- written/unwritten conversion;
- truncate and hole punch;
- FAIR node, parent, sibling, and root changes.

It is not used to represent SSD-to-NVM residency transitions. Consequently,
overwriting `[4, 5)` in SSD extent `[0, 8)` leaves that extent unchanged and
publishes one page descriptor. The three-entry split disappears from the
foreground path.

`ExtentRecordEntry` must eventually stop accepting NV data targets. It may
continue as the NVM-resident representation of SSD extents and child routes.
The existing FAST/FAIR structural protocol is retained for those roles. This
design supersedes the mixed SSD/NV entry portions of plans 014 through 017; it
does not invalidate their true tree-split or orderless-I/O requirements.

## 10. Concurrency and Locking

The common path uses redirect-index shard locks, baseline-range generations,
and exact page pins. It does not take an inode-wide publication guard or leaf
mutation guard.

No redirect shard lock is held during NVM persistence, SSD allocation, BIO
submission, device flush, or extent-tree mutation. Operations use this order:

```text
snapshot/claim redirect state -> release shard
-> perform persistence or I/O
-> acquire shard -> compare exact versions -> publish runtime result
```

True SSD baseline changes take range coordination, publish the confirmed NVM
baseline, advance the range generation, and release their extent-tree lock
before redirect clearing. A concurrent foreground write can therefore publish
a newer redirect at any point; version comparison ensures completion never
removes it.

Truncate and hole punch require a persistent inode/range epoch or tombstone so
recovery and late completion cannot resurrect data beyond the new file mapping.
That range-deletion protocol is a prerequisite for enabling those operations,
not something inferred from absence in the DRAM index. Until that protocol is
specified and implemented, truncate, hole punch, and relocation retain the old
serialized path; the first implementation stage covers reads, ordinary
overwrites, and their migration only.

## 11. Crash States

| Durable state at crash | Read/recovery authority | Recovery action |
| --- | --- | --- |
| NV data written, descriptor selector not published | old redirect or SSD baseline | reclaim unreachable slot |
| new descriptor selected, old descriptor retained | highest logical version | rebuild index with new version; retire old when unpinned |
| migration target stored, SSD batch not flushed | NV descriptor | resubmit or allocate a replacement target |
| SSD batch flushed, confirmed NVM baseline unchanged | NV descriptor | repeat baseline publication or migration cleanup |
| new SSD baseline published, redirect retained | NV descriptor | verify matching version, then clear redirect |
| redirect selector cleared, slot not returned to free pool | SSD baseline | recover slot as free after reachability scan |
| old completion arrives after a newer write | newer descriptor | retire only exact old generation; never clear current redirect |

The descriptor selector is the redirect publication and removal commit point.
The DRAM contiguous-page-range index, dirty queue, and range epochs never
participate in recovery.

## 12. Migration From the Current Code

Implementation should be staged so correctness does not depend on switching all
consumers at once:

1. Make data recovery choose the greatest descriptor logical version per
   PageKey and reject ties.
2. Introduce the sharded page/range redirect indexes and rebuild them from data
   descriptors; keep current extent entries for cross-checking only.
3. Route reads through descriptor redirect first and SSD extent fallback.
4. Change foreground overwrite to publish descriptors without inserting NV
   extent entries; retain the old path behind test-only comparison until crash
   tests pass. Keep truncate, hole punch, relocation, and other range deletion
   on the old serialized path.
5. Change rTIO selection to dirty page ranges and exact descriptor pins. Remove
   full extent claim, persistent leaf reads, and data-page reverse locators.
6. Change common completion to batch-clear matching descriptors without leaf
   publication. Keep structural publication only for new or changed SSD
   mappings.
7. Remove NV variants from `ExtentRecordEntry` and delete obsolete reverse-index
   maintenance after all readers, recovery, and diagnostics use redirects.

No on-disk or NVDIMM format is silently reinterpreted. If descriptor semantics
or extent entry tags change incompatibly, bump the image version and reject old
images unless an explicit conversion tool is supplied.

## 13. Verification

Correctness tests must include:

- overwrite one page in the middle of an eight-page SSD extent and prove the
  extent-tree bytes and entry count do not change;
- mixed SSD/NVM residency inside one extent with correct reads for every page;
- publish 32 contiguous NVPages, rebuild the contiguous-page-range index after
  simulated crash, and recover one range without reading an extent leaf;
- overwrite part of a page range during migration and prove completion clears
  only the submitted old generations;
- mutate the SSD baseline range generation during migration and prove the fast
  path falls back to baseline re-resolution rather than clearing a stale
  redirect;
- crash at every descriptor data/copy/selector boundary and every migration
  target/batch-flush/baseline/clear boundary;
- hole write, delayed allocation, unwritten conversion, truncate, and hole punch
  with crashes around baseline publication;
- slot reuse ABA, logical-version overflow, duplicate recovery versions, and an
  older SSD BIO completing after a newer NVM publication;
- repeated recovery to prove cleanup is idempotent.

Performance validation uses the established 2 GiB workload, 64 MiB high
watermark, and 32 MiB low watermark for 4 KiB random write and 128 KiB
sequential write at one and sixteen threads. Diagnostics must separately count:

- foreground descriptor batches, persisted bytes, and fences;
- redirect-index lookup and shard-lock time;
- candidate page ranges/pages and claim time;
- persistent descriptor reads during candidate selection, expected to be zero;
- extent leaf reads and publications on existing-baseline migration, expected
  to be zero;
- completion version comparisons, descriptor-clear batches, and reclaimed
  contiguous slot ranges;
- true SSD baseline mutations for hole/allocation workloads.

The design succeeds functionally when ordinary overwrites and their migration
never mutate the extent tree, and succeeds operationally when the 128 KiB path
performs range-level index work plus bounded descriptor batches rather than one
leaf/reverse validation sequence per 4 KiB page.

## 14. Non-Goals

- Making the DRAM redirect index persistent.
- Removing per-page descriptors; they are the recovery identity and reclaim
  boundary.
- Promising one atomic 128 KiB user write. Successfully published page prefixes
  follow existing write semantics.
- Eliminating real SSD extent splits caused by allocation, truncate, or hole
  punch.
- Changing rNVPC's WAL-based update protocol.
