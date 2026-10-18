> **2026-10-18 仓库合并说明**：此文件保留恢复过程中修订过的历史状态。
> 当前代码位于父目录，当前 rebuild 实测结果见 [docs/recovery](../docs/recovery/)。
> 旧分支名、`../os_com` 和旧容器路径属于历史证据，不代表仍需维护另一代码仓库。

# Current Status

## Rebuild：EXT4 组级分配器（2026-10-17）

本条只描述 `/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild` 的当前工作树，不把下方 2026-10-13 的历史实现状态当作 rebuild 证据。

003 核心实现完成：独立组分配 gate、约 12KiB/完整组的分层空闲索引、统一 physical/inflight/rNVPCe 容量预留账本；组保护退出后再发布超级块计数，pending-free 由新鲜 journal pin 快照排除。没有新增 PA 或 delalloc。

EXT4/PageCache 回归 593 passed / 0 failed；真实 `/dev/nvme1n1p1` buffered fio 12 个样本各写满 16GiB；4GiB 重新挂载后 CRC32C 及累计 15 次 e2fsck 全部通过。顺序写两轮均值：单线程 321.8 -> 280.6 MiB/s（-12.8%），16 线程 299.8 -> 295.2（-1.5%）；没有稳定吞吐提升。诊断显示超级块持有均时 7.954 -> 2.676μs，但新增组 gate 等待均时 39.098μs。后续分析组内维护、journal capture 和组分布；扩展故障矩阵尚未全部完成。

默认无探针 ELF 与吞吐测试版本完全相同，既有无关改动保留，未提交或推送。完整证据：[实现报告](/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild/docs/recovery/EXT4-GROUP-ALLOCATOR-2026-10-17.md)；[003 计划](os_com_refactor_plan/003-buddy-cache-allocator/README.md)。

## Repositories

CPU isolation (2026-10-13): default-off boot switch applies affinity before
thread creation, with users0-31/flush60/completion61/virtio62/journal63, nice0.
Actual entry CPUs verified; eight uninstrumented NVMe random4K/16-job runs
finish256MiB at98.08-108.94MiB/s (median98.46), with no30+MiB/s sample.
This is a test placement option, not a general scheduler or affinity fix.
Evidence: [CPU isolation](test/aster-ext4-baseline-test/results/20261013-cpu-isolation/README.md).

Intermittent low bandwidth diagnosis (2026-10-13): uninstrumented repeats show
108.94/36.06/34.83/98.84MiB/s. Slow serial diagnostics identify16-19ms waiting
in the BIO software queue before virtqueue submission; host NVMe write await
stays0.08-0.10ms. Driver-thread scheduling is the leading explanation. A temporary
nice=-1 control gives98.46-98.84MiB/s in four runs, but three restored-default runs
also remain fast, so a permanent fix is not established. All probes and priority
changes are removed; default kernel rebuilt. Evidence:
[low bandwidth diagnosis](test/aster-ext4-baseline-test/results/20261013-lowbw-investigation/README.md).

Pending-index isolation (2026-10-13): completion pending lookup now uses64
inode-hashed directory shards and per-slot generation/committed admission, without
`pool.state`. Six targeted KVM tests and nine static checks pass; final three
bandwidth/two profile runs finish256MiB without completion errors. Pending lock
acquisition drops6.46-9.36us to0.20us; leaf processing25.07-27.65us to19.02us.
Uninstrumented busy samples109.40/32.61/33.73MiB/s, median33.73 vs90.14 before,
do not establish an end-to-end improvement. Initial stale-directory race was
fixed and covered by regression; those runs are excluded. Evidence:
[pending index](test/aster-ext4-baseline-test/results/20261013-pending-index/README.md).

Leaf internal profile (2026-10-13): two KVM/NVMe random4K/16-job runs finish256MiB.
Completion pending-mark lookup waits6.46-9.36us for `pool.state`, holds it only
0.37-0.38us; the wait contributes26-34% of leaf processing. Exclusive admission
is1.05-1.11us and two-buffer allocation0.38-0.44us per update. No mechanism change
or throughput improvement is claimed. Evidence:
[leaf internal breakdown](test/aster-ext4-baseline-test/results/20261013-leaf-detail/README.md).

Completed and tested (2026-10-13): completion uses leaf-level exclusion and an
independent reservation guard, without inode publication. Eight targeted KVM
tests and nine static checks pass; three uninstrumented and two instrumented
NVMe/16-job fio runs complete256 MiB. Busy median98.46MiB/s vs98.84 before is
essentially unchanged. Inode publication waiting is eliminated, but leaf
processing/reclamation still dominate; no end-to-end speedup is established.
The new code remains uncommitted for review. Evidence:
[leaf-level completion](test/aster-ext4-baseline-test/results/20261013-leaf-completion/README.md).
Current checkpoints: kernel `644fd298d`, workloads `3b48118ae`, docs `8de5530`.

Repair workspace phase 1 (2026-10-13): bounded cleared-frame reuse, unindexed
repair images and direct pending-mark cleanup retain the existing recovery
protocol. Two KVM recovery/reuse/capacity tests and eight static checks pass.
Extent update drops from 289-302us to 71-119us/write; foreground publication body
from 484-500us to 172-292us. Uninstrumented busy samples 28.13/98.84/108.94MiB/s
(median98.84) vs baseline76.42/82.58 (median79.50); variability remains, so 24.3%
is a sample-median comparison, not a stable speedup claim. Phase 2 is not included.
Evidence: [repair workspace](test/aster-ext4-baseline-test/results/20261013-repair-workspace/README.md).

Foreground publication profile (2026-10-13): two matching detailed NVMe
random4K/16-job runs measure 484-500us/write inside publication, with extent
modification about 60%. Ordinary leaf rewrite's repair-image/mark creation and
cleanup alone cost 50-51% of the body; user copying is 1.64-1.77us and SSD partial
reads are zero. Completion publication wait remains 251-269us/group. Shared
metadata lifecycle locks remain, but their exact contribution is not isolated.
Evidence: [foreground profile](test/aster-ext4-baseline-test/results/20261013-foreground-publication-profile/README.md).

Migration profile follow-up (2026-10-13): two matching instrumented NVMe
random4K/16-job runs show completion publication acquisition costs 247-250us/group
(40-51% of group time), with observed owners all Foreground. Per-inode reservation
ledger work is 6-9%; its exclusive-entry wait is about 0.70us. The shared reservation
metadata-index lock averages 19.4us across all roles; global SSD-credit release
lock averages 0.14-0.21us. Evidence:
[migration breakdown](test/aster-ext4-baseline-test/results/20261013-migration-breakdown/README.md).

Latest implementation: rNVPCe data completion now defers native EXT4 extent
materialization to explicit filesystem metadata sync. NV extent records remain
resident; full metadata eviction/SSD-only recovery is not implemented. Native-only
fallocate, regular-file unlink and rename replacement are rejected on rNVPCe.
Journal/nojournal KVM checkpoint/read tests and over-capacity fio pass. Three
uninstrumented runs measure 33.68/98.46/89.82 MiB/s (median 89.82); variability
prevents a stable speedup claim. Completion materialize/inner-wait counters are
zero. Evidence: [deferred materialization](test/aster-ext4-baseline-test/results/20261012-deferred-materialize/README.md).

Latest performance follow-up: uninstrumented fast NVMe random4K/16-job runs
measure 71.31/76.42/76.65 MiB/s (median 76.42), all complete 256MiB.
Instrumented completion reclamation is 5.90 us/page on fast repeat and
6.26 us/page on no-persist; materialization and reference publication now
dominate. The first fast sample has an unreproduced SSD durability stall and
is retained in the report. Evidence:
[merged reclaim profile](test/aster-ext4-baseline-test/results/20261012-merged-reclaim-profile/README.md).

Latest implementation: allocator free slots/capacity/sequences now use a separate
allocation lock. New rNVPCe completion uses one reclaim_after_ssd_publish call,
without pool control or legacy reference scans on success. Six targeted KVM
tests and eight static checks pass. Two fast NVMe random4K/16-job smoke runs
complete 256MiB at 29.80/94.81 MiB/s; large variability prevents a speedup claim.
Counters remain disabled. Evidence:
[allocation and combined reclaim](test/aster-ext4-baseline-test/results/20261012-allocation-reclaim/README.md).

Reclaim validation follow-up: keep full runtime validation before descriptor
clear; finalization now checks the retained lease invariants with debug assertions
and unconditionally clears reverse. Failure/retry and slot-reuse KVM tests pass;
performance follow-up is linked above. Correctness evidence:
[single validation](test/aster-ext4-baseline-test/results/20261012-reclaim-single-validation/README.md).

Current focus: performance experiments. Normal kernels exclude the durable-image
allocation/copy code and reject `rnvpc_sim_mode=model`; use fast or
fast-no-persist. Historical crash-model fixtures remain ktest-only for future
work; crash-consistency validation is deferred. Earlier fast benchmarks already
used a single image. Ordinary backend io_lock has now been removed from data,
user-copy and descriptor accesses; only explicit legacy model fixtures retain
image locking. ObjectRecord/reverse now share one lock per slot; migration
identity validation and reverse lookup use a single acquisition. Slot isolation
and lifecycle/bounds KVM tests pass. NVMe random4K/16-job fio measures
52.78 MiB/s (no-persist), 45.71 and 43.61 MiB/s (fast, last on final source).
These are diagnostic samples, not an established speedup; counters are disabled.
Evidence: [per-slot state and combined lookup](test/aster-ext4-baseline-test/results/20261012-per-slot-reverse/README.md).

Instrumented per-slot follow-up: completion service is 99.38 us/page (fast)
and 91.56 us/page (no-persist). Reclamation dominates at 71.38%/65.91%;
legacy reference checks and slot/capacity return still use pool control locking.
Slot-lock acquisition inside reclamation is only 0.31%/0.38% of completion.
One diagnostic sample per mode; not an isolated before/after optimization test.
Evidence: [current completion profile](test/aster-ext4-baseline-test/results/20261012-per-slot-profile/README.md).

Historical narrow change (now superseded by combined reclaim): finish_after_ssd_publish no longer scanned the legacy
reference table. Generation/lease/state checks and the shared free_data_page_if_seq
check remain. The slot-lifecycle KVM test passes; performance has not been rerun.
Evidence: [skip legacy finish check](test/aster-ext4-baseline-test/results/20261012-skip-legacy-reference-check/README.md).

Two directed KVM tests and eight static checks pass after backend-lock removal.
Real NVMe random4K/16-job fio completes at 52.89 MiB/s (no-persist) and
40.31 MiB/s (fast), one diagnostic sample per mode with counters disabled.
Evidence: [backend without io_lock](test/aster-ext4-baseline-test/results/20261012-backend-no-io-lock/README.md).

| Repository | Branch / revision | Status |
| --- | --- | --- |
| `../os_com` | `rnvpce-soft-update`, checkpoint `3b48118ae` | Direct writes and global-queue/reverse-index migration are committed; tested leaf-level completion changes remain uncommitted. |
| `.` | documentation branch | Existing user-owned test-harness changes remain dirty and were not modified by this task. |

## Inode-local Publication (2026-10-12)

Native EXT4 inodes now cache their own publication owner/wait queue. Foreground
writes and completion no longer acquire the global pool control lock to claim
or release publication; a separate weak directory serves initialization and
standalone/recovery callers. Three directed KVM tests and eight static checks
pass. Four real NVMe random-4K/16-job fio points completed: busy bandwidth is
38.04/33.68 MiB/s with simulated persistence disabled and 17.85/24.15 MiB/s in
default fast mode. These overlap prior variability and do not establish a
stable throughput improvement. Counters are disabled; final boot mode is fast.
Evidence: [inode-local publication](test/aster-ext4-baseline-test/results/20261012-inode-publication/README.md).

Instrumented follow-up: completion service is 173.46 us/page in fast and
106.01 us/page without simulated persistence. In the latter, reference
publication/reservation is 40.56%, reclamation 46.66%, materialization 9.04%,
and SSD durability 2.64%. One diagnostic sample per mode, not a stable speedup
claim or an isolated lock-wait measurement.
Evidence: [stage profile](test/aster-ext4-baseline-test/results/20261012-inode-publication-profile/README.md).

## DRAM Persistence Ablation (2026-10-12)

Opt-in `rnvpc_sim_mode=fast-no-persist` skips simulated persist/fence operations
while retaining range/epoch validation and ordinary backend I/O locking.
Default fast and model recovery behavior remain unchanged. With latency counters
disabled, NVMe random 4K/16 jobs busy throughput is 23.79/17.84 MiB/s in fast
versus 33.68/34.83 MiB/s in the ablation (alternating, two runs each).
This identifies simulator overhead, not the hardware cost of clflush/sfence or
an isolated measurement of io_lock contention. Both directed KVM tests and all
four fio points passed. The final build parameters are restored to fast.
Evidence: [persist ablation](test/aster-ext4-baseline-test/results/20261012-persist-ablation/README.md).

Instrumented follow-up: completion service falls from 157.85 to 116.18 us/page.
With simulated persistence disabled, reclaim is 47.32%, reference publication
39.27%, native metadata materialization 9.19%, SSD durability 3.01%.
Descriptor clear falls from 52.12 to 9.92 us/page; legacy reference checks and
slot return together cost 41.84 us/page. These intervals include control-lock
waits, which are not yet measured separately from the work inside the lock.
Evidence: [completion profile](test/aster-ext4-baseline-test/results/20261012-persist-completion-profile/README.md).

## NVPool Object-State Sharding (2026-10-11)

Historical step (superseded by per-slot locking): `records + reverse` shared
64 slot-indexed locks; allocation/capacity and the
remaining control maps keep their original lock. Per-object lifecycle transitions
remain joint operations; the aggregate pin count is atomic telemetry only.
Five directed KVM tests and eight completion-context static checks pass.
NVMe random 4K, 16 jobs, 64 vCPUs, 256M dataset and 64M NVPool: normal busy
throughput improves from 10.85 to 24.95/25.00 MiB/s. Instrumented mean reclaim
lock acquisition falls from 33.225 to 0.246 us; per-page completion service falls
from 379.77 to 158.15 us. Diagnostics are disabled and the final ordinary kernel
was rebuilt and tested. No commit was created by this change.
Evidence: [pool-shards report](test/aster-ext4-baseline-test/results/20261011-pool-shards-validation/README.md).

## Foreground Checkpoint

The rNVPC comparison object now uses a persistent per-inode WAL and a fully persistent per-inode B-link B+tree. The WAL capacity is 1024 records. Record 1025 is blocked while the calling application thread replays exactly the preceding 1024 records in LSN order; no same-page coalescing and no background WAL-checkpoint thread are used. SSD completion is independent of WAL retirement.

| Mechanism | Status | Evidence |
| --- | --- | --- |
| Persistent layouts and metadata update protocol | Complete | `ffc3d288b`..`6cb08f8da` |
| Persistent WAL and local B-link update/split | Complete | `deb980d43`..`88da2d8a` |
| Foreground full-WAL checkpoint | Complete | `3f1946a83` |
| rTIO/WAL decoupling | Complete | `4711e88de` |
| Recovery, mapping rebuild, and orphan cleanup | Complete | `d0e89bf65`..`28a4abc6a` |
| Stable checkpoint counters | Complete | `1bd8eadd0` |
| Clean-path recovery-scan removal | Complete | `fc2c1735c` |
| Image-backed QEMU smoke workload | Complete | `f0184385d` |

## Validation

- Normal `cargo osdk check --package aster-kernel`: PASS.
- Ktest compile `cargo osdk check --ktests --package aster-kernel`: PASS.
- Full `make ktest ENABLE_KVM=1`: PASS; all 800 `aster-kernel` tests completed with zero failures.
- Counter gate: PASS; record 1025 produces one 1024-record checkpoint and `wal_max_used=1024`.
- rNVPCe regression: PASS; WAL/checkpoint counters remain zero.
- Image-backed KVM smoke: PASS for 8 MiB random 4 KiB and 256 MiB sequential 128 KiB writes, with final completion marker.

The smoke used a disposable image, not `/dev/sda3` or `/dev/nvme1n1p1`. Its modeled pool was 1 GiB with 128 MiB metadata reserve and 768 MiB data limit so temporary foreground/writeback duplicate versions could not exhaust the correctness fixture.

### Superseded rNVPCe Batch-Publication Validation

- These results validate the currently implemented legacy batch-publication prototype, not the accepted final rNVPCe design.
- Publication-fault, unpublished-object cleanup, rollback visibility, and selected-authority tests passed and remain useful as historical regression evidence while the implementation is replaced.
- Image-backed 4 MiB random-4-KiB orderless fio smoke: PASS with `FIO_EXIT_STATUS: 0`, drain completion, and `ASTER_BASELINE_COMPLETE`.
- Final-model validation must cover fixed-inode recovery roots, crash-consistent prefixes, pending-mutation repair, root-traced leak reclamation, and repeated-recovery idempotence.

### rNVPCe Task 4 Validation

- Normal and ktest-config `cargo osdk check --package aster-kernel`: PASS.
- Image-backed 4 MiB random-4-KiB fio: PASS for buffered/orderless, direct, and fsync modes; each reports `FIO_EXIT_STATUS: 0`, drain completion, and `ASTER_BASELINE_COMPLETE` with the default kernel stack.
- The stack fault was removed structurally: 4 KiB extent records are heap-backed on read, and recursive traversal no longer shares a frame with large node-construction temporaries.

### rNVPCe Task 5 Validation

- `cargo osdk check --package aster-kernel`: PASS.
- `cargo osdk test rnvpc`: PASS for the complete rNVPC/rNVPCe recovery regression filter.
- Initial-write modeled restart covers allocation-only, data-durable, extent-reference-durable, and fixed-inode-visible prefixes; unreachable prefixes are reclaimed and the visible prefix remains readable.
- Split and root-split tests cover `Prepared`, parent publication, `ParentPublished`, old-child repair, mark clear, and contradictory topology rejection at the recovery boundary.
- Repeated recovery preserves the same free-page count and exact generation-checked references; cleared mutation and repair frames are returned to the pool free-list.
- Recovery rejects cyclic/misleveled topology, leaf mappings outside parent routing intervals, and multiple reachable NV generations for one logical page instead of exposing an ambiguous DRAM index.

### rNVPCe Task 3 Validation

- `cargo osdk check`: PASS.
- `cargo osdk test rnvpce_`: PASS for the complete Task 3-prefixed regression set.
- `cargo osdk test rnvpc_`: PASS for mode isolation, publication locking, and existing rNVPC regressions.
- Split topology TDD: malformed new-child and old-child routes are rejected during direct publication and Prepared-mark recovery.
- The unfiltered ktest suite was not completed in this session because the existing `ext4::checksum::crc32c_microbench` stalls under the TCG/TSC environment; focused suites and compilation are green.

## rNVPCe Soft Update

The active `rnvpce-soft-update` branch replaces the comparison object's WAL/B-link publication path with WAL-free soft updates. The accepted task order is recorded in [`rNVPC-rTIO/rnvpce-soft-update-implementation-plan.md`](rNVPC-rTIO/rnvpce-soft-update-implementation-plan.md).

| Task | Status | Evidence |
| --- | --- | --- |
| Asynchronous rTIO completion context | Complete | `307f34b75` |
| Final-form persistent metadata and fixed inode roots | Complete | Typed records use stable `(slot, allocation_seq)` references; fixed inode records are updated in place and are the only recovery roots. |
| Authoritative NV metadata and local soft updates | Complete | Extent nodes are updated locally in place; repair images plus `PendingExtentMutation` make split/rewrite/root-growth cuts recoverable. |
| Durable VFS write path | Complete | rNVPCe reads and writes bypass PageCache data, copy directly between userspace/SSD and NVPages, and return only after the soft-update prefix is durable. |
| Recovery and leak reclamation | Complete | Recovery repairs pending mutations before exposure, validates exact root-reachable references, reclaims incomplete prefixes, and is idempotent across repeated modeled restarts. |
| rTIO migration mechanism | Implemented, runtime gate pending | Global dirty-NVPage queue; 8-byte reverse locator; migration-time SSD allocation; asynchronous merged BIO submission; IRQ enqueue plus process-context completion; old inode-scanning `rtio.rs` removed. |
| rTIO evaluation | Pending | Image-backed lifecycle/fault matrix and retained FIO `busy_bw_mib_s` results remain. |

### rNVPCe Task 6 Validation

- Fresh normal `cargo osdk check`: PASS; only the pre-existing unused `PageCache::evict_range` warning remains.
- Fresh ktest-configuration `cargo osdk check`: PASS.
- Static IRQ/completion-context suite: PASS, 7/7; the callback performs no persistent mutation, allocation, reservation update, reclaim, or in-flight credit return before queue drain.
- Focused TCG VM ktests: PASS, 2/2 across separate runs. `rnvpce_reverse_migration_publishes_ssd_and_consumes_reservation` covers reverse publication, SSD transition retry, and single reservation consumption; `rnvpce_migration_credit_release_survives_persistent_ledger_failure` proves that a failed persistent-ledger refresh still releases live credit exactly once.
- Source-level tests additionally cover count-only reservation, split-range freeze, dirty-queue reconstruction, and modeled restart, but the complete focused filter was not run in a VM during this session.
- KVM is unavailable in this session. Consequently the image-backed over-capacity fio/mount/unmount gate and all performance claims remain pending.
- rNVPCe resize after fixed-root publication remains explicitly rejected with `EOPNOTSUPP` until persistent suffix deletion and SSD reservation reclamation are implemented.

## Remaining Work

Completion leaf batching is implemented (2026-10-11): successful durable BIO pages
are regrouped by inode/current leaf, with at most 128 pages per publication window,
one combined leaf update and one reservation-ledger refresh per group. Two focused
KVM tests and eight IRQ-context checks pass. The instrumented NVMe random-4K/16-job
comparison reports busy throughput 6.23 -> 10.41 MiB/s and amortized completion
service 701.77 -> 379.77 microseconds/page; these are single-run results. See
[`completion-leaf-batching-analysis.md`](test/aster-ext4-baseline-test/results/20261011_230050_aster-ext4-baseline/completion-leaf-batching-analysis.md).
Diagnostics-disabled repeat completes 256 MiB at 10.85 MiB/s (`20261011_230249`).

- Implement rNVPCe persistent suffix deletion so truncate can atomically remove NV/SSD extent references and reclaim both SSD reservations and NV frames.
- Run dedicated performance matrices after correctness integration; report formal throughput using `busy_bw_mib_s`.
- Real-NVDIMM power-loss validation remains out of scope. The DRAM backend proves protocol behavior under modeled restart/fault injection, not hardware persistence.

## Documentation

The completed rNVPC comparison object is recorded in [`rNVPC-rTIO/implementation.md`](rNVPC-rTIO/implementation.md). The active rNVPCe work is tracked in [`rNVPC-rTIO/rnvpce-soft-update-implementation-plan.md`](rNVPC-rTIO/rnvpce-soft-update-implementation-plan.md). Historical design evolution remains under [`archive/design/`](archive/design/).
