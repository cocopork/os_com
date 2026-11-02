# 020 rTIO Multi-worker Migration

Date: 2026-11-02

Status: Approved for implementation. The first version applies only to
rNVPCe (`RnvpcExtend`) mode; rNVPC WAL migration remains single-worker.

## 1. Intent

rTIO currently uses one migration worker. `Ext4::migrate_nv_round()` holds the
filesystem-wide `nv_migration_lock` across candidate selection and BIO
submission, so a second worker could not make progress even if it were
started. Completion also issues one device flush for every migration batch.
Simply duplicating the current worker would therefore add scheduler and flush
contention without creating useful I/O parallelism.

The multi-worker design has two goals:

1. Migration of one NV extent entry must not block a foreground write to a
   different extent entry.
2. When foreground writers wait for NVPage capacity, multiple migration
   workers must claim and submit independent extent entries concurrently so
   reclaimable capacity becomes available sooner.

The design preserves the existing orderless migration protocol. It changes
runtime scheduling and batching, not the persistent object format or recovery
authority rules.

## 2. Scope

The first implementation covers rNVPCe (`RnvpcExtend`) mode, where foreground writes
already publish the SSD allocation and logical SSD baseline before migration.
Each dirty NV extent entry therefore contains a contiguous logical range, a
contiguous NVPage range, and a contiguous SSD target range.

The following remain outside this implementation:

- rNVPC WAL candidate and completion parallelization;
- virtio-blk multiqueue support;
- multiple durability coordinator threads;
- changing the persistent redirect-entry format;
- changing allocation-metadata authority or recovery rules;
- inode or locality-group preallocation.

## 3. Pipeline

The runtime pipeline has three stages:

```text
Migration workers (N)
  claim distinct dirty NV extent entries
  wait only for writers already inside those entries
  submit asynchronous data BIOs
                    |
                    v
Durability coordinator (1)
  collect every fully completed migration batch currently ready
  issue one device flush for the collected durability epoch
  transition each successful entry Migrating -> IoComplete
  publish reclaim work items
                    |
                    v
Reclaim workers and foreground helpers
  clear matching persistent redirects
  release contiguous NVPage ranges
  wake capacity waiters
```

A **durability epoch** is the set of migration batches removed from the ready
completion queue before one device flush begins. All data BIOs in the epoch
have completed. One successful device flush establishes durability for all of
them. Batches becoming ready after the queue snapshot belong to the next
epoch. The epoch exists only in DRAM; after a crash, persistent redirect state
continues to select NVM and recovery safely retries migration.

## 4. Migration Worker Pool

rNVPCe supports a bounded number of migration workers. The default is:

```text
1
```

The kernel command-line parameter `rtio_workers=N` may override the default
for controlled experiments. Valid values are 1 through 16. Invalid values are
rejected during configuration rather than silently clamped. rNVPC WAL mode
uses one worker regardless of this parameter until its per-page completion
path is redesigned.

The initial real-device validation used a single virtio-blk queue. Four
submitters increased submission-side contention and capacity-wait time, so
multiple workers remain opt-in until the device path provides multiple queues
or a centralized submission layer.

Every worker follows the existing wake policy. Dirty-watermark pressure,
explicit migration-progress waiters, or any capacity waiter wakes all
migration workers. A successful submission immediately attempts another round
while work remains. An empty selection waits for the next migration epoch or
completion notification; it does not poll continuously.

The existing experimental CPU isolation already places user threads and
kernel workers in disjoint CPU sets and places the virtio submission thread on
its own CPU. The first version retains that policy. Dedicated per-worker CPU
pinning is an evaluation option, not a correctness requirement.

## 5. Candidate Ownership

The filesystem-wide `nv_migration_lock` is not used to serialize rNVPCe
candidate selection. `RedirectDirectory::claim_ready_entries()` already uses
partition-local locks and an atomic state transition:

```text
Dirty -> Migrating
```

Only one worker can win that transition for a given `DirtyNvExtentEntry`.
Different workers can claim different entries concurrently. The rotating
partition cursor distributes their starting points, while failed state
transitions skip entries already owned by another worker.

Candidate discovery still takes an inode-map spinlock and the selected inode's
entry-map spinlock while it scans that map. A foreground lookup of another
entry in the same inode can therefore wait for this short in-memory critical
section. The guarantee is that no writer waits for another entry's migration,
BIO submission, completion, or device flush; it is not a claim that unrelated
lookups execute without any shared-memory lock contention. The implementation
records claim-lock latency. Replacing the map scan with a ready-entry queue is
deferred unless measurements identify this critical section as significant.

After claiming an entry, the worker waits for foreground writers that entered
that same entry before the state transition. New foreground writes cannot
acquire its write guard while it is `Migrating`. No inode-wide publication
lock, extent-tree lock, or unrelated entry lock is held during this wait.
Consequently:

- foreground writes to the claimed entry wait and retry;
- foreground writes to another entry of the same inode continue;
- foreground writes to another inode continue.

Each worker limits one round to the existing 1024-page budget. The budget is
per worker, not a filesystem-wide in-flight limit. NVPage capacity and the
number of claimable entries provide natural backpressure.

## 6. Migration Quiescence

The old `nv_migration_lock` also protected sync-time state transitions, so it
cannot merely be deleted. It is replaced by a reader/writer migration gate:

- each rNVPCe migration round holds a shared permit from candidate claim
  through BIO submission;
- sync/unmount acquires the exclusive permit, preventing new worker rounds and
  waiting for active submitters to leave;
- the exclusive guard authorizes a sync-owned submission entry point, which
  migrates dirty entries without reacquiring the shared side of the gate;
- while retaining the exclusive permit, sync uses that entry point until no
  dirty candidate remains, drains in-flight batches and reclaim work, and then
  exports native EXT4 extents and allocation metadata;
- rNVPC WAL retains an additional exclusive single-worker lock around its
  legacy round.

No foreground write acquires this gate. The gate therefore cannot serialize
foreground writes with migration except through the per-entry state described
above.

Allocation-metadata writeback continues to use block-group-local ownership and
must not take a filesystem-wide migration lock. Current rNVPCe migration does
not allocate SSD blocks or modify allocation metadata: foreground writes have
already allocated the target and updated NVM-authoritative allocation state.
It may therefore coexist with allocation-metadata writeback. Foreground
allocation and the metadata pager still coordinate through the selected
group's `AllocationWritebackState`.

## 7. Durability Coordination

BIO callbacks remain minimal and interrupt-safe. They record status in their
`MigrationBatch`, decrement its request count, and enqueue the batch only when
all its data BIOs have completed.

The single durability coordinator drains the ready queue into one local epoch.
For rNVPCe (`RnvpcExtend`) batches:

1. Separate failed-I/O batches from successful batches.
2. If at least one successful batch exists, issue exactly one device flush.
3. If the flush succeeds, transition every entry in every successful batch to
   `IoComplete` and publish reclaim work.
4. If the flush fails, retain NVM authority, restore/requeue all affected
   leases, and fail migration according to the existing mount-wide error
   policy.
5. Return `nv_in_flight` credit only after the batch has reached a terminal
   completion decision.

The coordinator does not wait for migration workers that still have BIOs in
flight. This bounds durability latency. If several workers finish close
together, their batches share one flush; otherwise they form separate epochs.
No fixed delay is introduced merely to enlarge an epoch.

rNVPC WAL batches retain their existing completion and persistence path. They
must not be mixed into an rNVPCe durability epoch in the first implementation.

## 8. Reclaim and Capacity Feedback

After the epoch flush, completion only marks the entry I/O-complete and
publishes a `ReclaimWorkItem`. Persistent redirect removal and NVPage release
remain outside the durability coordinator.

Foreground threads waiting for capacity may continue to claim reclaim work,
and the background reclaim worker processes remaining work. Reclaiming one
entry releases its entire contiguous NVPage range and wakes capacity waiters.
This keeps the feedback path:

```text
capacity wait -> wake all migration workers -> parallel BIO submission
              -> one durability flush -> parallel/helped reclaim
              -> capacity wakeup
```

The durability coordinator must not perform the potentially longer redirect
cleanup and NVPage free operations, because doing so would delay processing of
later completed batches.

## 9. Failure and Crash Consistency

The persistent authority invariant is unchanged:

- before the durability flush succeeds, the redirect selects NVM;
- after the flush but before redirect reclamation, both copies may exist but
  the persistent redirect still makes the state recoverable;
- redirect removal is persisted before NVPage allocation state is cleared;
- a crash discards worker, epoch, queue, and lock state, then recovery rebuilds
  dirty entries from persistent metadata and retries incomplete migration.

One worker or BIO failure marks migration failed for the mounted filesystem,
matching the existing policy. Other already-submitted batches are still
drained so their leases and in-flight accounting are not abandoned. No worker
may release another worker's entry lease.

## 10. Locking Rules

The required lock order and blocking rules are:

1. The shared migration gate may cover candidate claim and BIO submission, but
   no foreground path acquires it.
2. Redirect partition/inode-map spinlocks are held only for lookup, state
   transition, or queue mutation; no wait or device I/O occurs under them.
3. Waiting for existing entry writers occurs after all redirect-index locks
   are released.
4. BIO submission and device flush occur without redirect-index, inode,
   extent-tree, allocation-group, or NVPage object-table locks held.
5. The completion-queue spinlock is held only while moving queue entries.
6. Reclaim operates on one entry lease at a time and does not hold the
   durability coordinator lock.
7. Sync acquires the exclusive migration gate before draining the pipeline;
   workers never acquire it recursively.

## 11. Validation

Correctness tests must demonstrate:

- two concurrent shared migration permits are allowed, while sync's exclusive
  permit waits for both;
- two calls can claim disjoint NV extent entries without duplicate ownership;
- two completed rNVPCe batches drained in one epoch issue one device flush;
- an I/O or flush failure retains NVM authority and releases no NVPage;
- sync prevents new submissions, drains in-flight work, and exports a
  consistent native baseline;
- rNVPC WAL remains single-worker and passes its existing recovery tests.

Performance validation uses the existing 2 GiB workload and 128 MiB rNVPC
pool. Compare `rtio_workers=1` with the default worker count for:

- 4 KiB random write at 1 and 16 jobs;
- 128 KiB sequential first write at 1 and 16 jobs.

Record fio bandwidth, sync-after bandwidth, capacity-wait latency, candidate
selection time, submitted pages/BIOs, maximum in-flight pages, durability epoch
batch/page counts, device-flush count, reclaim throughput, and `e2fsck` result.
The expected result is lower capacity-wait time and higher sustained migration
throughput under pressure. A fio bandwidth increase without reduced capacity
wait is insufficient evidence that the design met its primary goal.
