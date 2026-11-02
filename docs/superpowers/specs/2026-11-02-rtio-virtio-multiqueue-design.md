# rTIO Worker-affine VirtIO Block Multi-queue Design

## 1. Intent

The current rTIO worker pool can claim independent dirty extent entries, but
all workers enqueue into one software queue, one forwarding thread, and one
virtqueue. Real-device validation showed that four producers increased BIO
submission time from 0.89 s to 4.74 s and capacity-wait time from 1.26 s to
18.16 s for the 128 KiB, 16-job diagnostic point.

This design makes the submission path parallel end to end:

```text
rTIO worker 0 -> BIO queue affinity 0 -> software queue 0 -> virtqueue 0
rTIO worker 1 -> BIO queue affinity 1 -> software queue 1 -> virtqueue 1
...
completion IRQs from all queues -> shared rTIO durability coordinator
```

Each rTIO worker continues to scan, claim, construct, and submit its own BIOs.
There is no centralized BIO submitter.

## 2. Goals

- Different rTIO workers submit through different virtqueues without sharing
  queue, request-ID, or completion-map locks.
- Keep one stable `worker_id -> submission_queue_id` mapping for the lifetime
  of a mounted filesystem.
- Preserve entry-granular exclusion: unrelated foreground writes do not wait
  for a worker's SSD I/O.
- Preserve one device FLUSH per ready durability epoch.
- Make queue selection part of the common block interface and transparently
  preserve it through partitions.
- Map different virtqueues to different host QEMU IOThreads and host NVMe
  blk-mq hardware contexts in the real-device experiment, rather than treating
  a negotiated guest queue count as evidence of host-side parallelism.
- Keep single-queue block drivers and callers source-compatible through the
  default `Any` queue selection.

## 3. Non-goals

- rNVPC WAL remains single-worker.
- This change does not add persistent fields or alter rNVPCe recovery.
- It does not guarantee cross-virtqueue request ordering.
- It does not implement a generic Linux blk-mq scheduler, request stealing,
  NUMA topology, or dynamic worker migration.
- It does not make the durability coordinator multi-threaded.

## 4. Common Block Interface

Add a queue selection value to BIO submission metadata:

```rust
pub enum BioQueueAffinity {
    Any,
    Queue(u16),
}
```

`Bio::new` keeps producing `Any`. Add `Bio::new_on_queue` and queue-aware
variants of the asynchronous block helpers, including
`write_segments_on_queue_async`. `SubmittedBio` exposes the affinity as a
read-only value. Partition forwarding changes only the sector offset and must
preserve queue affinity.

Extend `BlockDeviceMeta` with:

```rust
pub nr_submission_queues: u16
```

The value is always at least one. `Queue(id)` is a strict request: `id` must be
less than the advertised count, otherwise enqueue returns a new
`BioEnqueueError::InvalidQueue`. A single-queue driver accepts queue 0 and
continues to use its existing path. Test and memory devices may ignore the
distinction after validating the index.

`Any` is driver-selected. VirtIO uses the submitting CPU modulo the negotiated
queue count, which spreads ordinary filesystem I/O without changing callers.
rTIO never uses `Any`; it supplies the worker's stable queue ID.

## 5. VirtIO Feature Negotiation

Add `VIRTIO_BLK_F_MQ` to the supported feature set. When negotiated, read the
modern `num_queues` configuration field and initialize:

```text
min(device_config.num_queues, transport.num_queues, DRIVER_QUEUE_LIMIT)
```

`DRIVER_QUEUE_LIMIT` is 16 in the first version to bound DMA and kernel-thread
resources. Without MQ, with a legacy transport that cannot expose the field,
or when capability preflight cannot produce a valid queue count, initialize
exactly one queue. This fallback decision is made before configuring any
virtqueue. After multi-queue configuration starts, queue setup and callback
registration are all-or-nothing: any failure aborts device initialization.
This version does not reset the transport and attempt a second initialization
with queue 0.

Every selected queue is independently validated against
`transport.max_queue_size(queue_id)`. The block device advertises the minimum
segment limit supported by all active queues.

The QEMU test device must use `num-queues=N`; merely setting this option is not
sufficient unless MQ was negotiated and all queues were initialized.

## 6. Per-queue Runtime State

Replace the single outer staging queue and the shared inner queue state with:

```rust
struct VirtioBlockQueue {
    staging: BioRequestSingleQueue,
    virtqueue: SpinLock<VirtQueue>,
    request_headers: Arc<DmaStream>,
    responses: Arc<DmaStream>,
    request_ids: SyncIdAlloc,
    submitted: SpinLock<BTreeMap<u16, SubmittedRequest>>,
    descriptor_waiters: WaitQueue,
}
```

`BlockDevice` owns `Vec<Arc<VirtioBlockQueue>>`. Queue selection happens once
in `BlockDevice::enqueue`; merging is local to that staging queue, so requests
from different queue affinities never merge.

Each queue gets its own descriptor ring, request/response DMA slots, request ID
allocator, submitted-request map, IRQ callback, and forwarding thread. No
global virtqueue or completion-map lock remains on data submission.

The submission ordering within a queue is:

1. reserve a request ID;
2. prepare header, data DMA, and response slot;
3. add descriptors and obtain the token;
4. insert `token -> SubmittedRequest` in that queue's completion map;
5. notify the device.

The completion record must be visible before notification so an immediate IRQ
cannot observe a token without an owner. If descriptors are unavailable, the
forwarding thread sleeps on the queue's descriptor wait queue; its IRQ handler
wakes that queue after consuming used descriptors.

## 7. IRQ and Forwarding Threads

Register one queue callback for every active virtqueue. The callback captures
the queue ID and drains only that queue. Shared PCI interrupt vectors remain
valid because each callback checks its own used ring.

The device registry spawns one forwarding thread per active queue and calls
`handle_requests(queue_id)`. Under experimental CPU isolation, queue threads
receive deterministic disjoint CPU affinities where enough CPUs exist. CPU
pinning is an optimization, not part of correctness; queue affinity is carried
by the BIO and remains stable even if an rTIO worker migrates between CPUs.

## 8. rTIO Worker Mapping

At mount, rNVPCe reads `nr_submission_queues` from the selected block device.
The effective worker policy is:

```text
default workers = min(4, max(1, online_cpu_count / 8), submission_queues)
```

`rtio_workers=N` remains the explicit override. For rNVPCe, an override larger
than `nr_submission_queues` is rejected with `EINVAL`; silently mapping two
workers to one queue would recreate the measured contention. rNVPC WAL always
uses one worker.

Worker `i` owns queue `i` and passes `Queue(i)` to every data BIO in its
migration round. Worker IDs and queue IDs are runtime-only. Candidate claims
remain atomic `Dirty -> Migrating`, so workers cannot submit the same dirty
extent entry.

## 9. Host Backend and NVMe Hardware Queues

The performance harness uses QEMU 9.0 or newer. QEMU 9.0 introduced
`iothread-vq-mapping` for assigning virtio-blk queues of one disk to different
IOThreads; the host's installed QEMU 8.2.2 does not expose this property and is
retained only as a single-IOThread control.

For an N-queue experiment, the harness creates N IOThreads and maps virtqueue
`i` exclusively to IOThread `i`. After QEMU starts, QMP `query-iothreads`
provides each host thread ID. The harness reads
`/sys/block/nvme1n1/mq/*/cpu_list`, selects N host CPUs that map to distinct
NVMe blk-mq hardware contexts, and pins one IOThread to each selected CPU. The
target remains a raw, direct host partition; no host filesystem or image cache
is inserted between QEMU and the authorized NVMe partition.

This mapping is a performance configuration, not a filesystem correctness
dependency. The experiment must record both mappings:

```text
rTIO worker -> guest virtqueue -> QEMU IOThread/host CPU -> NVMe hctx
```

Host tracing at the NVMe queue submission point is the authority for whether
multiple hardware queues were actually used. Queue counts, configured CPU
affinity, or guest diagnostics alone are insufficient.

## 10. Completion and Durability

BIO callbacks from every virtqueue publish their `MigrationBatch` to the same
completion queue. The existing single durability coordinator drains all ready
batches as one epoch.

VirtIO queues are serviced out of order and this design assumes no cross-queue
ordering. The durability sequence is therefore:

1. wait until every data BIO belonging to a batch has completed;
2. collect all currently ready successful batches from all queues;
3. submit one device FLUSH through queue 0;
4. after FLUSH completion, transition those entries to `IoComplete` and
   publish reclaim work.

The VirtIO block specification requires a completed FLUSH to commit previously
completed writes to stable storage. Newer writes may overlap the flush and may
also become durable; they are not reclaimed until a later epoch, so this is
safe. A flush failure leaves all epoch entries NVM-authoritative. A data BIO
failure affects only its batch and drops its lease back to `Dirty` after the
epoch decision.

Sync first closes the migration gate, waits for all worker submission rounds,
drains all queue completions and reclaim work, then performs the existing
allocation metadata and extent export.

## 11. Shutdown and Error Handling

- Queue initialization is all-or-nothing before the driver reaches
  `DRIVER_OK`; partial initialization returns an error.
- A queue-local enqueue failure does not fall back to another queue, because
  that would violate stable worker affinity and hide overload.
- Synchronously observable submission/device errors and injected queue/IRQ
  failures fail the mounted migration pipeline through the existing
  `nv_migration_failed` path and preserve NVM authority. Detecting a submitted
  request whose IRQ never arrives would require a watchdog and is outside this
  version.
- VirtIO forwarding threads are device-lifetime kernel threads, as they are in
  the current driver. Filesystem shutdown quiesces and drains rTIO, but this
  change does not introduce a generic block-device unregister/shutdown API.
- Single-queue behavior remains the fallback for devices without MQ.

## 12. Diagnostics

Add per-queue counters without per-page timing:

- queued and submitted BIOs/pages;
- staging-queue depth and maximum depth;
- descriptor wait count/time;
- active and peak forwarding threads;
- IRQ completions;
- rTIO worker-to-queue mapping;
- per-queue in-flight requests;
- durability epoch batches/pages and device flush count.

The performance report must compare queue balance and verify that no queue
receives all rTIO traffic when multiple workers are enabled.

## 13. Verification

### Common block layer

- `Any` remains accepted by all existing drivers.
- exact queue IDs are preserved through `PartitionNode`.
- invalid queue IDs return `InvalidQueue` without changing a staging queue or
  invoking the BIO completion callback.
- request merging never crosses queue affinity.

### VirtIO driver

- negotiate MQ only when offered and read the configured queue count;
- initialize and register callbacks for queues 0..N;
- two producers targeting different queues can enqueue and complete without
  sharing queue state;
- immediate completion cannot race ahead of completion-map insertion;
- descriptor exhaustion sleeps and is woken by its queue's IRQ;
- single-queue fallback passes the existing block tests.

### rTIO

- two workers use different queue IDs and claim different extent entries;
- one durability epoch containing completions from multiple queues issues one
  FLUSH;
- mixed success/failure and flush-failure tests retain correct authority and
  in-flight accounting;
- sync quiesces all queue-bound workers;
- rNVPC WAL remains on one worker and queue.

### Real-device experiment

Use otherwise identical release images with QEMU `num-queues=1` and
`num-queues=4`. The four-queue arm uses QEMU 9.0+ with four IOThreads, one
virtqueue per IOThread, and four host CPUs mapped to distinct NVMe hardware
contexts. Run 4 KiB random write and 128 KiB sequential first write at 1 and
16 jobs, 2 GiB per point, reformatting and running read-only e2fsck each time.
Capture guest per-queue counters, QEMU IOThread IDs/affinity, the host
CPU-to-hctx map, and host NVMe request counts per hctx. The mechanism is
accepted only if:

- four workers actually use four queues;
- at least four distinct NVMe hardware contexts issue requests in the
  four-queue arm, with no single context receiving more than 70% of traced
  requests; the one-queue control must remain separately identified;
- there is no correctness failure;
- `nv_data_submit` contention and cumulative capacity wait fall relative to
  the previous four-worker/single-queue result;
- 16-job including-sync throughput improves without a material 1-job
  regression.

If these conditions fail, keep the runtime default at one worker and retain
multi-queue only as an experimental path.

## 14. Standards Basis

VirtIO 1.3 section 5.2 defines request queues 0 through N-1 when
`VIRTIO_BLK_F_MQ` is negotiated, states that requests may be serviced out of
order, and defines FLUSH as the stable-storage completion point for previously
completed writes. The implementation must not infer cross-queue ordering that
the standard does not provide.

QEMU 9.0 added support for processing different virtio-blk queues of one disk
on different IOThreads. Linux blk-mq maps per-CPU software submission contexts
to device hardware contexts; therefore the host experiment pins IOThreads to
CPUs selected from distinct NVMe hctx `cpu_list` files and verifies the result
at the NVMe submission point.
