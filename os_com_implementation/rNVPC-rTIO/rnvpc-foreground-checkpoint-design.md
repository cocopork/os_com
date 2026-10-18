# rNVPC Foreground WAL Checkpoint Design

> Status: approved design, implementation pending. Approved on 2026-09-28.

## Purpose

rNVPC is the Corundum-style comparison object in AchillesFS. It models a
persistent PageCache protected by a per-inode WAL and a fully persistent
PageCache B+tree. It deliberately does not use a background WAL checkpoint
thread. When an inode WAL is full, the application thread that needs the next
record synchronously checkpoints the full WAL before continuing its write.

This mechanism is separate from rNVPCe. rNVPCe remains WAL-free and publishes
NVPage/NVBH references through soft-update ordering. The expected performance
difference is an experiment question, not an implementation guarantee.

## Persistent Objects

Each active inode owns these self-describing NVPool objects:

- InodePcAnchor: B+tree root, height, WAL geometry, monotonic LSN state,
  checkpointed LSN, generation, and checksum.
- PersistentWalRecord: LSN, Upsert operation, logical page index, old and new
  NVPage identities and generations, SSD backing block, checksum, and commit
  word.
- BTreeNode: inode and node identity, level, page LSN, high key, right sibling,
  generation, checksum, and internal or leaf entries.
- NVPage: page data and its existing self-describing persistent descriptor.

The B+tree is per inode. Its leaf mapping is:

~~~text
page_index -> (nv_page, nv_generation, ssd_backing_block)
~~~

The anchor, WAL frames, and B+tree nodes use typed NV metadata payloads. They
contain no Rust pointers, locks, runtime reference counts, or DRAM handles.
Recovery discovers them by scanning committed NV metadata descriptors and
payload type tags; no global SSD-block-to-NV-object index is introduced.

## Write Path

One per-inode mutation lock serializes mapping publication and checkpoint. A
normal write performs:

~~~text
lock mutation_lock
  ensure one free WAL record
  allocate and fill the new NVPage
  persist NVPage; fence
  allocate an LSN and persist the WAL record and commit word; fence
  update the inactive anchor image with WAL tail, used, and next_lsn
  persist and select the new anchor image; fence
  publish the new version in the DRAM PageCache index
  mark the NVPage wal_pinned
unlock
return success
~~~

The WAL record and the anchor image that exposes its tail are durable before
the DRAM mapping is published. A write cannot return success unless both its NVPage and committed WAL record are persistent.
The old version remains available until checkpoint makes the new mapping part
of the persistent B+tree.

## Foreground Checkpoint

The WAL capacity remains 1024 records per inode. The write that needs record
1025 performs checkpoint in its own application-thread context while holding
that inode's mutation lock:

~~~text
read committed WAL records from head to tail
for each record in increasing LSN order:
    replay one B+tree Upsert without coalescing records for the same page
persist and fence all affected B+tree nodes
write checkpointed_lsn, WAL head, and wal_used=0 into one inactive anchor image
persist and atomically select that anchor image; fence
unpin new NVPages covered by the checkpoint
move replaced versions to the reclaimable set
continue the triggering write
~~~

There is no background WAL checkpoint thread. rTIO neither initiates checkpoint
nor retires WAL records. A checkpoint failure leaves the persistent WAL head
unchanged and causes the triggering write to return an error. Earlier successful
writes remain recoverable from their NVPages and committed WAL records.

## Persistent B+tree

The index is a per-inode persistent B-link B+tree. Updates are local and
in-place at the logical-node level; the implementation must not flatten or
rebuild the whole tree and must not copy the complete root-to-leaf path.

Each logical node stores two local images plus an active selector:

~~~text
BTreeNodeFrame
  stable header
  image[0] = (generation, page_lsn, entries, checksum)
  image[1] = (generation, page_lsn, entries, checksum)
  active_slot
~~~

A node update writes and persists the inactive image, fences, then atomically
switches and persists active_slot. Recovery therefore has at least one valid
node image even if a metadata write is interrupted.

A split uses this order:

1. Allocate, initialize, and persist the right node.
2. Persist the left node's new high key and right-sibling pointer.
3. Insert and persist the parent separator.
4. Recursively split parents when required.
5. For a root split, persist the new root before switching the anchor root.

Search follows right-sibling links when the key exceeds a node's high key. A
crash between steps 2 and 3 therefore leaves the new right node reachable, and
WAL replay can idempotently finish the parent insertion. Every replay carries
the record LSN; a node whose page_lsn already covers that operation is not
modified again.

InodePcAnchor is also double-imaged. Initial inode state is published in this
order: empty root, WAL frames, then anchor. An anchor never names an
uninitialized root or WAL.

## rTIO Boundary

An NVPage referenced only by an uncheckpointed WAL record is wal_pinned. rTIO
skips such pages because writing them to SSD cannot reclaim their NV capacity
safely. After checkpoint publishes the persistent B+tree mapping, the page
becomes a normal NV-to-SSD candidate.

SSD BIO completion changes only page writeback and reclamation state. It must
not advance the WAL head, release WAL capacity, or define checkpoint completion.
Failed SSD completion preserves the NVPage and retry eligibility.

## Recovery

Recovery performs:

1. Scan and validate committed anchors, WAL frames, B+tree nodes, and NVPages.
2. Group objects by inode and select the highest valid anchor generation.
3. Recover the root and choose each node's highest valid local image.
4. Validate page LSNs, high keys, and right-sibling reachability.
5. Read committed WAL records with LSN greater than checkpointed_lsn from head
   to tail.
6. Replay every record in LSN order, including repeated updates to one page.
7. Persist the repaired tree, advance checkpointed_lsn and WAL head, and fence.
8. Rebuild the DRAM PageCache index and rTIO candidate state.
9. Reclaim objects not referenced by an anchor, B+tree, or committed WAL record.

A WAL tail entry without a valid commit word belongs to an unsuccessful write
and is ignored. A crash after durable head advancement but before old-object
reclamation can leak space temporarily but cannot lose a mapping; the orphan
scan reclaims that space later.

## Concurrency And Locking

- Same-inode write and checkpoint operations serialize on mutation_lock.
- Different inodes can write and checkpoint concurrently.
- Capacity admission occurs before lower-level page or metadata-frame locks.
- Checkpoint performs no SSD I/O and does not wait for rTIO completion.
- rTIO must not wait for mutation_lock while holding page, frame, or device
  completion locks.
- Recovery runs before normal foreground access and before rTIO starts.

## Required Implementation Changes

- Replace the DRAM-only WAL ring with persistent WAL frames and durable anchor
  state.
- Add a persistent per-inode B-link B+tree implementation.
- Replace request_writeback_and_wait with foreground checkpoint_if_full.
- Remove WAL retirement from SSD completion and rTIO accounting.
- Add wal_pinned eligibility checks to rTIO candidate collection.
- Extend recovery to rebuild from anchors, trees, and committed WAL records.
- Keep rNVPCe unchanged and WAL-free.

## Validation

Correctness tests must cover:

- record 1024 succeeds and record 1025 triggers checkpoint in the caller;
- all 1024 records replay in order, including repeated keys;
- NVPage persistence precedes WAL commit and WAL commit precedes publication;
- checkpoint failure never advances WAL head;
- fault injection around node-image persistence, selector switch, every split
  step, root publication, and anchor head advancement;
- same-inode serialization and different-inode checkpoint concurrency;
- rTIO exclusion before checkpoint and eligibility after checkpoint;
- SSD completion does not change WAL head or used count; and
- orphan collection preserves every object reachable from tree or WAL.

Performance evaluation records WAL append latency, foreground checkpoint
latency, records replayed, node updates, splits, persist fences, writes blocked
by checkpoint, WAL occupancy, and write latency percentiles. Detailed timing
instrumentation remains disabled for normal throughput runs. Results must state
that NV persistence is modeled and cannot prove real-PM crash behavior.
