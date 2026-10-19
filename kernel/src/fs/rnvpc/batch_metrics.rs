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
}

struct Timing {
    calls: AtomicU64,
    ns: AtomicU64,
    max_ns: AtomicU64,
}

static TIMINGS: [Timing; 7] = [const {
    Timing {
        calls: AtomicU64::new(0),
        ns: AtomicU64::new(0),
        max_ns: AtomicU64::new(0),
    }
}; 7];
static EVENTS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

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
    if ENABLED {
        EVENTS[event as usize].fetch_add(n as u64, Ordering::Relaxed);
    }
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
