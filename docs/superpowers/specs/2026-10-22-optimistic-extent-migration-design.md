# Optimistic Extent Migration With FAST/FAIR Persistence

Date: 2026-10-22

## Intent

Separate two concerns that the current `recovery/rnvpce-fast-fair` work mixed
together:

1. FAST/FAIR defines how an extent-tree mutation remains crash-consistent.
2. rTIO defines how foreground writes and background NV-to-SSD migration run
   concurrently.

The migration path shall return to the optimistic model represented by
`recovery/rnvpc-rebuild` at `5bdff5380`: no inode publication guard, leaf
mutation guard, or range gate remains held across SSD I/O. FAST/FAIR field-level
persistence and sibling-first tree split remain the extent update protocol.

## Required Behavior

### Candidate and submission

rTIO selects dirty NVPages from the physical dirty queue and groups them by the
current `(inode, leaf, extent entry)`. It validates and claims a complete NV
extent, transitions every member page generation from `NvDirty` to `InFlight`,
and pins those exact generations. Claiming does not retain an inode publication
guard or leaf mutation guard.

SSD target allocation, descriptor target publication, asynchronous data BIOs,
EXT4 allocation-metadata BIOs, and the batch device flush follow the existing
orderless migration protocol. No extent metadata lock is held while these I/Os
are outstanding.

### Foreground writes

Remove the inode-wide `0..u64::MAX` migration gate and the range-gate checks
added to `write_rnvpce_at`. Foreground writes to unrelated pages and extents of
the same inode may proceed while migration I/O is outstanding.

An `InFlight` physical NVPage remains immutable. A foreground operation must
never modify that page in place. To match the old branch's implemented behavior,
a write overlapping that physical generation releases inode/publication/mutation
state, waits for the migration lease to change, and retries from the current
authoritative mapping. It does not allocate and immediately publish a competing
NVPage generation. Capacity waits likewise occur without inode or extent locks.

### Completion

After all data and allocation metadata requests complete, completion issues one
device flush for the migration batch. It then acquires metadata locks only for
the short validation-and-publication interval, using the same global lock order
as foreground mutation.

Completion works per NV extent rather than per 4 KiB page. Its expected identity
contains:

- filesystem and inode identity;
- logical start and length;
- expected NV base slot and member allocation generations;
- the descriptor-published contiguous SSD target run;
- the candidate's locator as a hint, not permanent identity.

Completion re-resolves the current leaf because a concurrent FAST/FAIR split
may move the entry. Under the leaf mutation guard it validates once that the
current entry still denotes the complete expected NV extent and that all pinned
page generations and SSD targets match. On success it performs one aligned 8B
FAST target store from the NV base to the SSD base, then releases reservation
credit and batch-reclaims all member NVPages.

"Validates once" means one tree lookup and one guarded leaf/entry validation.
The implementation may traverse the extent's descriptor vector once to verify
member allocation generations and contiguous SSD targets; it must not repeat
tree lookup, leaf read, or leaf locking per member page.

If the extent was replaced, split incompatibly, redirected, or partially
superseded while I/O was outstanding, completion must not publish its SSD
target or reclaim any page still reachable from the current tree. It aborts the
whole extent claim and returns its still-current page generations to `NvDirty`;
it does not fall back to partial per-page publication. A stale locator is
resolved again and retried before declaring the identity stale.

SSD blocks already named by durable NVPage migration descriptors remain owned
by those descriptors and are reused when the still-reachable extent is requeued.
Completion neither frees a possibly named block nor turns it into an online
leak. An unreachable or contradictory identity is a migration failure requiring
the existing recovery reconciliation rather than speculative block release.

The common lock order is:

```text
foreground: inode.inner -> inode publication -> leaf guard
claim/completion:          inode publication -> leaf guard
```

Claim may hold completion publication briefly while it locates the extent and
transitions all members to `InFlight`, but releases publication and leaf guards
before target allocation or I/O. No path may acquire inode publication or
`inode.inner` while already holding a leaf guard.

## Crash Consistency

This change does not restore repair-image node updates.

- Ordinary NV-to-SSD completion changes only the aligned 8B `ExtentTarget`.
  Before the store, NVM is authoritative. After the persisted store, SSD is
  authoritative.
- Entry insert/delete/range changes retain FAST ordered field updates.
- Node split retains FAIR sibling initialization, sibling-link publication,
  old-node truncation, and later parent routing publication.
- Recovery interprets partial FAST/FAIR states from the tree itself. It does
  not require a full-node repair image for the normal migration path.
- Existing repair-image fallback for unsupported complex structural paths is
  tracked separately and must not be reintroduced into ordinary completion.

## Code Scope

Remove the uncommitted extent-migration lease implementation from:

- `kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- `kernel/src/fs/fs_impls/ext4/migration.rs`
- `kernel/src/fs/rnvpc/frame.rs`
- `kernel/src/fs/rnvpc/mod.rs`
- `kernel/src/fs/rnvpc/write_transaction.rs`

Retain the committed FAST/FAIR layout, target word, sibling routing, contiguous
NV extent representation, batched descriptor allocation, and in-place extent
mutation code introduced after `recovery/rnvpc-rebuild`.

The old repair branch is behavioral reference for concurrency and retry only.
Its whole-leaf reconstruction, per-page completion publication, and repair-image
mutation protocol must not be copied into the new completion implementation.

## Acceptance

- No inode publication guard, leaf guard, or inode-wide range gate spans SSD I/O.
- A foreground write to a non-overlapping extent of the same inode progresses
  during migration; a write that touches an `InFlight` generation waits outside
  inode/publication/leaf locks and retries.
- An overlapping write never modifies an `InFlight` NVPage in place and cannot
  cause stale completion to publish or reclaim the new authority.
- A 32-page contiguous NV extent uses one completion identity validation, one
  8B target publication, and batch reclaim rather than 32 leaf validations.
- Concurrent FAIR leaf split relocation is resolved once at extent granularity.
- Crash cuts before and after the target store expose complete NV or complete
  SSD authority, never a dangling extent target.
- The 4 KiB random and 128 KiB sequential 1/16-job performance and phase matrix
  is rerun against the saved repair-image baseline and the current lease result.

## Non-goals

- Reintroducing full-node repair images for ordinary entry updates.
- Holding a complete inode gate during migration.
- Allowing in-place writes to a physical NVPage while that generation is
  `InFlight`.
- Solving fragmented SSD targets; persistent NV extent split remains a separate
  protocol task.
