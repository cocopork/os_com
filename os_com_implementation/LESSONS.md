# Verified Engineering Lessons

These are reusable, evidence-backed lessons rather than a progress log. Current
work status belongs in [`STATUS.md`](STATUS.md), and detailed chronology stays
in the linked documents.

## Correctness And Persistence

**Symptom:** a completion or recovery path can release a newly reused object.
**Root cause:** identity checks that compare only a recycled page or frame ID
cannot distinguish an old completion from a new owner.
**Rule:** couple identity with a generation and make validation plus release one
atomic pool operation.
**Evidence:** [`ext4_performance.md`](ext4_performance.md) documents the
generation/ABA failure and the single-lock repair.

**Symptom:** an operation returns successfully while durable state is unclear.
**Root cause:** an API return value alone does not establish the persistence
boundary or identify the surviving copy after a failed transition.
**Rule:** specify the persistent point, authority copy, and recovery behavior;
test state after each relevant failure boundary.
**Evidence:** [`ext4_ktest_design_principle.md`](ext4_ktest_design_principle.md)
and [`rNVPC-rTIO/design.md`](rNVPC-rTIO/design.md).

## Extent And Allocation

**Symptom:** ordinary extent edits become expensive as a file grows.
**Root cause:** a whole-tree flatten and rebuild scales with unrelated extents.
**Rule:** keep common edits bounded and in-place; isolate rare rebuild paths and
measure them separately.
**Evidence:** [`ext4-function-check.md`](ext4-function-check.md) distinguishes
the bounded hot path from the remaining rebuild cases.

**Symptom:** writeback reaches allocation with insufficient space or metadata.
**Root cause:** delayed allocation defers physical assignment, so accounting
must reserve resources before writeback.
**Rule:** validate reservations, allocation, extent state, and durable metadata
together, including resource-exhaustion paths.
**Evidence:** [`ext4-function-check.md`](ext4-function-check.md) and
[`ext4_ktest_design_principle.md`](ext4_ktest_design_principle.md).

## PageCache And Writeback

**Symptom:** background writeback stalls foreground work or leaves data dirty.
**Root cause:** waiting for each submission serializes device work; unbounded or
coarsely synchronized dirty-state processing also creates contention.
**Rule:** submit work asynchronously, reclaim it at the completion boundary,
and keep dirty/writeback accounting observable and bounded.
**Evidence:** [`ext4_performance.md`](ext4_performance.md) and
[`os_com_refactor_plan/002-background-writeback/README.md`](os_com_refactor_plan/002-background-writeback/README.md).

## BIO And Completion Context

**Symptom:** contiguous buffered writes consume disproportionate CPU time and
device queue work.
**Root cause:** per-page BIO construction leaves no deterministic opportunity
to form larger requests.
**Rule:** identify contiguous runs while constructing BIOs, respect segment
limits, and verify both BIO shape and logical completion for every page.
**Evidence:** [`ext4_performance.md`](ext4_performance.md) and
[`ext4-function-check.md`](ext4-function-check.md).

**Symptom:** completion work deadlocks, races, or distorts the hot path.
**Root cause:** completion callbacks are treated as a general worker context.
**Rule:** keep callbacks short, defer heavy state changes, and test the
happens-before relation with explicit synchronization rather than timing.
**Evidence:** [`ext4_ktest_design_principle.md`](ext4_ktest_design_principle.md).

## Virtualization And Devices

**Symptom:** virtual-machine results stop scaling before the filesystem path is
fully exercised.
**Root cause:** the virtio block path can be constrained by a single queue and
per-segment descriptor use.
**Rule:** distinguish filesystem BIO aggregation from lower-layer descriptor
costs before assigning a throughput limit to one layer.
**Evidence:** [`ext4-function-check.md`](ext4-function-check.md).

## Experiment Methodology

**Symptom:** a striking throughput change cannot be reproduced or attributed.
**Root cause:** configuration drift, incomplete guest output, instrumentation,
or a single unstable observation is mistaken for evidence.
**Rule:** retain the exact environment and completion evidence, compare one
factor in the same session, repeat the observation, and report formal
throughput through `busy_bw_mib_s` only.
**Evidence:** [`EXPERIMENTS.md`](EXPERIMENTS.md),
[`ext4_performance.md`](ext4_performance.md), and
[`ext4_ktest_design_principle.md`](ext4_ktest_design_principle.md).
