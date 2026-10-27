// SPDX-License-Identifier: MPL-2.0

//! Compile-time optional migration counters. Throughput images leave this off.

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const ENABLED: bool = false;

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum Phase {
    Allocate,
    AllocationBarrier,
    TargetPublish,
    DataBarrier,
    LeafPublish,
    CreditPublish,
    Reclaim,
    RedirectPublish,
    RangeClaim,
    CompletionCompare,
    DescriptorClear,
    StructuralBaseline,
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum Event {
    AllocatedPages,
    TargetPages,
    CreditPages,
    ReclaimedPages,
    DataPersist,
    ControlPersist,
    Fence,
    RedirectPublishedPages,
    RedirectPublishBatches,
    CandidateRanges,
    CandidatePages,
    CompletionComparisons,
    DescriptorClearBatches,
    ReclaimedSlotRanges,
    StructuralBaselineMutations,
}

struct Timing {
    calls: AtomicU64,
    ns: AtomicU64,
    max_ns: AtomicU64,
}

static TIMINGS: [Timing; 12] = [const {
    Timing {
        calls: AtomicU64::new(0),
        ns: AtomicU64::new(0),
        max_ns: AtomicU64::new(0),
    }
}; 12];
static EVENTS: [AtomicU64; 15] = [const { AtomicU64::new(0) }; 15];

pub(crate) fn now() -> u64 {
    if ENABLED {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

pub(crate) fn elapsed(phase: Phase, started: u64) {
    if !ENABLED {
        return;
    }
    let ns = now().saturating_sub(started);
    let timing = &TIMINGS[phase as usize];
    timing.calls.fetch_add(1, Ordering::Relaxed);
    timing.ns.fetch_add(ns, Ordering::Relaxed);
    timing.max_ns.fetch_max(ns, Ordering::Relaxed);
}

pub(crate) fn count(event: Event, n: usize) {
    if ENABLED || cfg!(ktest) {
        EVENTS[event as usize].fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(ktest)]
pub(crate) fn reset_for_test() {
    for event in &EVENTS {
        event.store(0, Ordering::Relaxed);
    }
}

#[cfg(ktest)]
pub(crate) fn event_count(event: Event) -> u64 {
    EVENTS[event as usize].load(Ordering::Relaxed)
}

/// Emits cumulative snapshots; the final sync snapshot includes all migrations.
/// Error level avoids enabling per-syscall info logs in the diagnostic image.
pub(crate) fn dump() {
    if !ENABLED {
        return;
    }
    for (name, timing) in [
        "allocate",
        "allocation_barrier",
        "target_publish",
        "data_barrier",
        "leaf_publish",
        "credit_publish",
        "reclaim",
        "redirect_publish",
        "range_claim",
        "completion_compare",
        "descriptor_clear",
        "structural_baseline",
    ]
    .iter()
    .zip(&TIMINGS)
    {
        ostd::error!(
            "[NVPAGE-BATCH] phase={} calls={} sum_ns={} max_ns={}",
            name,
            timing.calls.load(Ordering::Relaxed),
            timing.ns.load(Ordering::Relaxed),
            timing.max_ns.load(Ordering::Relaxed)
        );
    }
    for (name, event) in [
        "allocated_pages",
        "target_pages",
        "credit_pages",
        "reclaimed_pages",
        "data_persist",
        "control_persist",
        "fence",
        "redirect_published_pages",
        "redirect_publish_batches",
        "candidate_ranges",
        "candidate_pages",
        "completion_comparisons",
        "descriptor_clear_batches",
        "reclaimed_slot_ranges",
        "structural_baseline_mutations",
    ]
    .iter()
    .zip(&EVENTS)
    {
        ostd::error!(
            "[NVPAGE-BATCH] event={} count={}",
            name,
            event.load(Ordering::Relaxed)
        );
    }
}
