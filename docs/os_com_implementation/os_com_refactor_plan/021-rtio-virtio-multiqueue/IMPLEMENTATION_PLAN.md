# rTIO VirtIO-Block Multi-queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an end-to-end queue-affine rTIO path from each rNVPCe migration worker through a distinct Asterinas virtqueue and QEMU IOThread to a verified distinct host NVMe hardware context.

**Architecture:** BIO queue affinity is carried by the common block API and preserved by partitions. The virtio-blk driver owns independent per-queue staging, DMA, descriptor, request-ID and completion state; rTIO binds worker `i` to queue `i`, while one coordinator retains device-wide FLUSH ordering. A QEMU 9.0+ real-device harness maps each virtqueue to a dedicated host IOThread and verifies actual NVMe hctx use with host tracing.

**Tech Stack:** Rust `no_std`, Asterinas block and virtio components, EXT4/rNVPCe/rTIO, VirtIO 1.3, QEMU 9.0+ QMP/IOThreads, Linux blk-mq, bpftrace, Docker/KVM, fio and e2fsprogs.

**Spec:** `docs/superpowers/specs/2026-11-02-rtio-virtio-multiqueue-design.md`

## Global Constraints

- Do not change persistent rNVPCe formats, authority rules, recovery scanning, or the FAST/FAIR soft-update protocol.
- rNVPC WAL remains single-worker; only rNVPCe (`RnvpcExtend`) gets queue-affine parallel migration.
- Strict `Queue(id)` submission never silently falls back; `Any` preserves compatibility for ordinary callers.
- VirtIO queue setup is all-or-nothing once multi-queue configuration starts; only capability preflight may choose the one-queue fallback.
- Data BIOs may complete out of order across queues; reclaim is allowed only after the shared durability epoch's device FLUSH succeeds.
- QEMU 8.2.2 is not acceptable evidence for IOThread-per-virtqueue mapping; the multi-hctx arm requires QEMU 9.0+ and `iothread-vq-mapping`.
- Real-device tests may erase only the already authorized 256 GiB `/dev/nvme1n1p1`, or `/dev/nvme1n1p2` after confirming the selected partition is unmounted and has the expected size.
- Never run destructive real-device points or repository builds concurrently.

## Review Focus

- Invalid strict queue IDs must fail without changing any staging queue or invoking the completion callback; Task 1 adds the rejection test.
- A partition must preserve affinity while changing only sector offset; Task 1 adds a forwarding test.
- An immediate IRQ must not race ahead of completion-map insertion, and descriptor exhaustion must sleep rather than spin; Task 3 adds both tests.
- A mixed-success cross-queue epoch or failed FLUSH must retain NVM authority and balanced in-flight accounting; Task 5 adds fault tests.
- Configured IOThreads may still reach one NVMe hctx; Task 7 requires submission-point hctx counts before accepting the mechanism.

---

### Task 1: Common block queue-affinity contract

**Files:**
- Modify: `kernel/comps/block/src/bio.rs`
- Modify: `kernel/comps/block/src/lib.rs`
- Modify: `kernel/comps/block/src/impl_block_device.rs`
- Modify: `kernel/comps/block/src/partition.rs`
- Modify: every `BlockDeviceMeta` constructor under `kernel/comps/{block,nvme,virtio,mlsdisk}/`
- Test: `kernel/comps/block/src/request_queue.rs`
- Test: `kernel/comps/block/src/partition.rs`

**Interfaces:**
- Produces: `BioQueueAffinity::{Any, Queue(u16)}`.
- Produces: `Bio::new_on_queue(...)`, `SubmittedBio::queue_affinity() -> BioQueueAffinity`.
- Produces: `BlockDeviceMeta::nr_submission_queues: u16` and `BioEnqueueError::InvalidQueue`.
- Produces: `write_segments_on_queue_async(bid, segments, affinity, complete_fn, io_batch)` on `dyn BlockDevice`.

- [ ] **Step 1: Write failing common-layer tests**

Add `bio_queue_affinity_defaults_to_any`, `strict_queue_rejection_does_not_enqueue_or_complete`, and `partition_preserves_queue_affinity_and_offsets_sector`. Assert the default, exact queue value, `InvalidQueue`, unchanged queue/callback counts, preserved queue ID, and adjusted sector range.

- [ ] **Step 2: Run each new test and verify RED**

Run each test as a separate `cargo osdk test <test-name> --qemu-args="-enable-kvm -cpu host -display none"` command. Expected: each fails because the affinity contract is absent.

- [ ] **Step 3: Implement the queue-affinity types and propagation**

Store affinity in `BioMetadata`, keep `Bio::new` as `Any`, add the strict constructor/helper, preserve it in `SubmittedBio`, and add `nr_submission_queues: 1` to non-MQ devices and test doubles. `PartitionNode::metadata()` inherits the underlying queue count.

- [ ] **Step 4: Run focused block tests and checks**

Run the three tests separately, then `cargo osdk check --package aster-block` and `cargo osdk check --package aster-kernel`. Expected: PASS with no missing metadata initializers.

- [ ] **Step 5: Commit**

```bash
git add kernel/comps/block kernel/comps/nvme kernel/comps/virtio kernel/comps/mlsdisk
git commit -m "Add block submission queue affinity"
```

### Task 2: VirtIO MQ negotiation and queue-count validation

**Files:**
- Modify: `kernel/comps/virtio/src/device/block/mod.rs`
- Modify: `kernel/comps/virtio/src/device/block/device.rs`
- Test: `kernel/comps/virtio/src/device/block/mod.rs`
- Test: `kernel/comps/virtio/src/device/block/device.rs`

**Interfaces:**
- Consumes: `BlockDeviceMeta::nr_submission_queues` from Task 1.
- Produces: `ConfigManager<VirtioBlockConfig>::num_queues() -> Result<u16, VirtioDeviceError>`.
- Produces: `active_queue_count(features, config_queues, transport_queues) -> Result<u16, VirtioDeviceError>` with `DRIVER_QUEUE_LIMIT = 16`.

- [ ] **Step 1: Write failing feature/configuration tests**

Add `virtio_blk_negotiates_mq_when_offered`, `virtio_blk_reads_modern_num_queues`, `virtio_blk_uses_one_queue_without_mq`, `virtio_blk_caps_queue_count_at_sixteen`, and `virtio_blk_falls_back_before_setup_when_mq_preflight_is_unusable`. Queue 0 itself remaining unusable is a device-initialization error rather than a fallback case.

- [ ] **Step 2: Run each test separately and verify RED**

Use one `cargo osdk test` invocation per test. Expected: MQ is filtered from `ALL_SUPPORTED`, `num_queues` is unread, or the queue-count helper is missing.

- [ ] **Step 3: Implement negotiation and preflight**

Add `MQ` to supported features, read `num_queues` only on the modern interface when MQ was negotiated, compute the bounded active count, and preflight every selected transport queue's maximum size before creating queue 0. Capability absence selects one queue; a failure after queue construction begins is returned without retry initialization.

- [ ] **Step 4: Run focused tests and component checks**

Run the five tests separately, then `cargo osdk check --package aster-virtio` and `cargo osdk check --package aster-kernel`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kernel/comps/virtio/src/device/block
git commit -m "Negotiate virtio block multiqueue"
```

### Task 3: Independent virtio-blk queue state, IRQs, and forwarders

**Files:**
- Modify: `kernel/comps/virtio/src/device/block/device.rs`
- Modify: `kernel/src/device/registry/block.rs`
- Modify: `kernel/src/thread/io_test_affinity.rs`
- Test: `kernel/comps/virtio/src/device/block/device.rs`
- Test: `kernel/src/thread/io_test_affinity.rs`

**Interfaces:**
- Consumes: `SubmittedBio::queue_affinity()` and active queue count.
- Produces: `VirtioBlockQueue` with queue-local staging, virtqueue, DMA slots, ID allocator, completion map, descriptor wait queue, and diagnostics.
- Produces: `BlockDevice::handle_requests(queue_id: u16)` and `BlockDevice::nr_submission_queues() -> u16`.
- Produces: `io_test_affinity::virtio_queue_affinity(queue_id, queue_count) -> CpuSet`.

- [ ] **Step 1: Write failing queue-isolation tests**

Add `strict_bios_enter_only_the_selected_staging_queue`, `any_bios_select_a_valid_cpu_local_queue`, `queue_completion_maps_are_independent`, `completion_owner_is_installed_before_notify`, `descriptor_wait_is_woken_by_its_queue_irq`, and `virtio_queue_cpu_sets_are_disjoint_when_capacity_allows`.

- [ ] **Step 2: Run each new test separately and verify RED**

Expected: tests fail because the driver has one queue, notifies before inserting its owner, busy-loops on descriptors, and exposes one Virtio CPU role.

- [ ] **Step 3: Introduce `VirtioBlockQueue` and route enqueue**

Move all queue-owned fields out of `DeviceInner`, build one state object per active queue, validate strict affinity, and select `Any` using current CPU modulo queue count. Preserve per-queue BIO merging by enqueueing into exactly one staging queue.

- [ ] **Step 4: Make submission and completion queue-local**

Pass queue state into read/write/flush and IRQ handlers. Insert `token -> SubmittedRequest` before `notify()`. Replace descriptor busy loops with the queue-local wait queue and wake it after used descriptors are consumed. Keep FLUSH available on queue 0.

- [ ] **Step 5: Spawn and place one forwarding thread per queue**

Change the registry loop to spawn `nr_submission_queues` threads calling `handle_requests(queue_id)`. Under `rtio_cpu_isolation=on`, assign deterministic distinct Virtio CPUs when available; otherwise allow shared CPUs without changing BIO queue affinity.

- [ ] **Step 6: Run tests, checks, and the single-queue regression**

Run each Task 3 test separately, `cargo osdk check --package aster-virtio`, `cargo osdk check --package aster-kernel`, and the existing block/virtio aggregate tests. Boot once with `num-queues=1` and verify existing EXT4 smoke behavior remains intact.

- [ ] **Step 7: Commit**

```bash
git add kernel/comps/virtio/src/device/block/device.rs kernel/src/device/registry/block.rs kernel/src/thread/io_test_affinity.rs
git commit -m "Run virtio block queues independently"
```

### Task 4: Bind rTIO workers to submission queues

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/pipeline_profile.rs`
- Test: `kernel/src/fs/fs_impls/ext4/migration.rs`

**Interfaces:**
- Consumes: `BlockDeviceMeta::nr_submission_queues` and `write_segments_on_queue_async`.
- Produces: `rtio_worker_count(mode, cpu_count, submission_queues, override_value) -> Result<usize>`.
- Produces: worker-loop calls to `migrate_nv_round_on_queue(page_budget, BioQueueAffinity::Queue(worker_id))`.

- [ ] **Step 1: Extend worker-policy tests and verify RED**

Update `rtio_worker_policy_defaults_and_bounds` to assert the approved default formula, `EINVAL` when the override exceeds available queues, and one worker for WAL/native modes. Add `rnvpce_workers_keep_stable_distinct_queue_ids`.

- [ ] **Step 2: Implement mount-time policy and stable worker IDs**

Read the mounted partition's advertised queue count before spawning workers. Capture each loop's `worker_id`, reject impossible overrides during mount, and pass strict queue affinity through only the rNVPCe migration data-BIO helper. Do not change normal writeback or WAL submission.

- [ ] **Step 3: Add per-worker/per-queue diagnostics**

Record worker-to-queue mapping, BIO/page counts, current and peak queue depth, descriptor waits, IRQ completions, and in-flight requests without adding per-page timestamps.

- [ ] **Step 4: Run focused tests and kernel checks**

Run `rtio_worker_policy_defaults_and_bounds` and `rnvpce_workers_keep_stable_distinct_queue_ids` separately, followed by `cargo osdk check --package aster-kernel` and `cargo osdk check --ktests --package aster-kernel`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kernel/src/fs/fs_impls/ext4/migration.rs kernel/src/fs/fs_impls/ext4/fs.rs kernel/src/fs/fs_impls/ext4/pipeline_profile.rs
git commit -m "Bind rTIO workers to virtio queues"
```

### Task 5: Cross-queue durability and failure validation

**Files:**
- Modify: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Modify: `kernel/src/fs/fs_impls/ext4/migration/batch.rs`
- Test: `kernel/src/fs/fs_impls/ext4/migration.rs`
- Test: `kernel/src/fs/fs_impls/ext4/nv_tests.rs`

**Interfaces:**
- Consumes: queue-affine worker submission from Task 4.
- Preserves: one shared `CompletionQueue`, one durability coordinator, and queue-0 device FLUSH.

- [ ] **Step 1: Write cross-queue epoch fault tests**

Add `rnvpce_cross_queue_epoch_issues_one_flush`, `rnvpce_cross_queue_mixed_data_error_preserves_failed_entry`, `rnvpce_cross_queue_flush_error_preserves_all_entries`, and `rnvpce_sync_quiesces_all_queue_bound_workers`. Assert entry state, NVM authority, reclaim publication, and zero balanced in-flight credit.

- [ ] **Step 2: Run each test separately and verify behavior before changing code**

Expected: the tests either fail because queue identity cannot be injected or expose an ordering/accounting defect. If they already pass after fixture extension, retain them as regression evidence and do not rewrite the coordinator.

- [ ] **Step 3: Make only the minimum coordinator changes required**

Ensure all queue callbacks feed the shared ready queue, snapshot successful batches before one queue-0 FLUSH, and restore only failed leases. Do not add cross-queue ordering assumptions or a second coordinator.

- [ ] **Step 4: Run focused and aggregate correctness tests**

Run the four tests separately, then `cargo osdk test ext4_research_routes --qemu-args="-enable-kvm -cpu host -display none"`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kernel/src/fs/fs_impls/ext4/migration.rs kernel/src/fs/fs_impls/ext4/migration/batch.rs kernel/src/fs/fs_impls/ext4/nv_tests.rs
git commit -m "Validate cross queue rTIO durability"
```

### Task 6: Reproducible QEMU IOThread and host-hctx harness

**Files:**
- Create: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/run-validation.sh`
- Create: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/select-hctx-cpus.sh`
- Create: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/trace-nvme-hctx.bt`
- Create: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/summarize.py`
- Create: `docs/experiments/scripts/rtio-virtio-multiqueue-20261102/README.md`

**Interfaces:**
- Produces: `QEMU_BIN` preflight requiring version 9.0+ and the `iothread-vq-mapping` property.
- Produces: a machine-readable mapping JSON containing guest queue, IOThread ID/TID, host CPU, and NVMe hctx.
- Produces: hctx request-count JSON captured at the host NVMe queue submission point.

- [ ] **Step 1: Write non-destructive harness self-tests**

Add `--self-test` cases for rejecting QEMU 8.2.2, selecting four unique hctx CPUs from a sysfs fixture, rejecting mounted/wrong-sized targets, constructing one-to-one `iothread-vq-mapping`, and failing acceptance when trace counts use fewer than four hctx or one hctx exceeds 70%.

- [ ] **Step 2: Run self-tests and verify RED**

Run `bash .../run-validation.sh --self-test`. Expected: FAIL because the scripts do not yet implement the checks.

- [ ] **Step 3: Implement QEMU and target preflight**

Require an explicit destructive-test flag, resolve the target's parent NVMe device, validate its 256 GiB size and unmounted state, record QEMU version/help, and refuse the multi-queue arm unless `iothread-vq-mapping` exists.

- [ ] **Step 4: Implement IOThread-to-hctx placement**

Create four IOThreads and JSON-map virtqueue `i` only to IOThread `i`. Use QMP `query-iothreads` to discover TIDs, choose CPUs from four distinct `/sys/block/<parent>/mq/<hctx>/cpu_list` entries, apply affinity with `taskset`, and save the complete mapping before starting fio.

- [ ] **Step 5: Implement authoritative host tracing**

Require the typed `kprobe:nvme_queue_rq` bpftrace probe, filter the selected parent disk through its request queue, and count requests by `blk_mq_hw_ctx.queue_num`. If the symbol or BTF fields are unavailable, fail the multi-hctx experiment instead of substituting CPU-distribution evidence. Save the probe source, raw output, kernel version, sysfs queue map, and parsed JSON.

- [ ] **Step 6: Run self-tests and shell/static checks**

Run `bash .../run-validation.sh --self-test`, `bash -n` on both shell scripts, and `python3 -m py_compile .../summarize.py`. Expected: PASS without touching a block device.

- [ ] **Step 7: Commit**

```bash
git add docs/experiments/scripts/rtio-virtio-multiqueue-20261102
git commit -m "Add virtio multiqueue hardware queue harness"
```

### Task 7: Bare-device correctness, scaling experiment, and report

**Files:**
- Create: `docs/experiments/reports/RTIO-VIRTIO-MULTIQUEUE-2026-11-02.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/021-rtio-virtio-multiqueue/README.md`
- Modify: `docs/os_com_implementation/os_com_refactor_plan/README.md`

**Interfaces:**
- Consumes: release image and harness from Tasks 1-6.
- Produces: correctness, queue-balance, hctx-use, latency, throughput and capacity-wait evidence for 1x1 and 4x4 configurations.

- [ ] **Step 1: Build and fingerprint the release image**

Run release checks, build once, record source commit/diff, ISO SHA256, QEMU version, container image digest, host kernel, NVMe model/firmware, scheduler and queue topology. Do not run another repository build during measurements.

- [ ] **Step 2: Run the one-queue control serially**

For 4 KiB random first write and 128 KiB sequential first write at jobs 1 and 16, run 2 GiB per point with 1 rTIO worker, 1 virtqueue and 1 IOThread. Reformat each point, require fio/sync/unmount success, and run `e2fsck -fn` afterward.

- [ ] **Step 3: Run the four-queue arm serially**

Repeat the same four points with 4 workers, 4 virtqueues, 4 one-to-one IOThreads, and 4 distinct selected NVMe hctx CPUs. Capture guest counters, QMP mapping, affinity, sysfs map, raw hctx trace, fio output and pipeline profile for every point.

- [ ] **Step 4: Check correctness and mechanism acceptance**

Require all fio/sync/unmount/e2fsck checks to pass, all four workers to submit to distinct virtqueues, at least four traced hctx to issue requests, no one hctx above 70%, and one device FLUSH per ready durability epoch. A failure blocks any multiqueue performance claim and leaves the default at one worker.

- [ ] **Step 5: Compare performance and bottleneck counters**

Report fio and including-sync MiB/s, mean and tail latency, `nv_data_submit`, capacity-wait time, per-queue depth/descriptor waits, flush count, per-hctx distribution, and 1-job regression. Only change the runtime default from one worker if the four-queue arm reduces submission contention/capacity waiting and improves 16-job including-sync throughput without a material single-job regression.

- [ ] **Step 6: Update status and commit evidence**

Update 021 and the refactor index with the exact implementation and validation status. Commit scripts and report only; do not add raw logs, images, generated plots, or large trace files.

```bash
git add docs/experiments/reports/RTIO-VIRTIO-MULTIQUEUE-2026-11-02.md docs/os_com_implementation/os_com_refactor_plan
git commit -m "Validate rTIO virtio multiqueue scaling"
```
