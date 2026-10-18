# 005 Virtio-Block Segment Merge

## Goal
Reduce virtqueue descriptor pressure by merging eligible contiguous segments.

## Current Status
**Implemented in the recovery rebuild worktree on 2026-10-18; not yet committed.** Historical filesystem-level BIO aggregation was distinct from this driver-side DMA descriptor merge. Historical status records are not evidence of a pre-crash implementation.

## Implemented Result
Read/write paths merge consecutive device DMA addresses within descriptor length limits while retaining all original BIO ownership, DMA synchronization and completion callbacks. Gapped buffers remain separate. Header and status descriptors are never merged with data.

## Verification Evidence
The real-DMA merge ktest passes (including gaps/order/alias/length-limit checks), as do 302 kernel ktests. Two 2 GiB / 128 KiB sequential-write / 16-job measurements reduce descriptor-blocked requests from 8023 / 8084 to 0 / 0; software waiting peaks decrease from 30 / 20 to 9 / 1. Full settings, limits and evidence are in [the rebuild report](/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild/docs/recovery/EXT4-VIRTIO-SEGMENT-MERGE-2026-10-18.md).

## Remaining Work
Optional follow-up: longer steady-state runs and additional workloads. Do not infer physical NVMe saturation from driver descriptor occupancy.

## Historical Notes
Do not combine this change with MQ work; each has a separate bottleneck model.
