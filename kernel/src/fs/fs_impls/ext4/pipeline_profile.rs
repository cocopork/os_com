// SPDX-License-Identifier: MPL-2.0

//! Optional cross-mode write-pipeline profiling for controlled experiments.

use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::prelude::*;

pub(crate) const ENABLED: bool = false;
const HISTOGRAM_BINS: usize = 48;

pub(crate) struct Metric {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    units: AtomicU64,
    histogram: [AtomicU64; HISTOGRAM_BINS],
}

impl Metric {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            units: AtomicU64::new(0),
            histogram: [const { AtomicU64::new(0) }; HISTOGRAM_BINS],
        }
    }

    pub(crate) fn record_since(&self, started_ns: u64, units: usize) {
        if !ENABLED {
            return;
        }
        self.record(now().saturating_sub(started_ns), units);
    }

    fn record(&self, elapsed_ns: u64, units: usize) {
        let bucket = histogram_bucket(elapsed_ns);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        self.max_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        self.units.fetch_add(units as u64, Ordering::Relaxed);
        self.histogram[bucket].fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
pub(crate) fn now() -> u64 {
    if ENABLED {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

const fn histogram_bucket(elapsed_ns: u64) -> usize {
    let bucket = 64 - elapsed_ns.saturating_sub(1).leading_zeros() as usize;
    if bucket < HISTOGRAM_BINS {
        bucket
    } else {
        HISTOGRAM_BINS - 1
    }
}

pub(crate) static NATIVE_FOREGROUND_TOTAL: Metric = Metric::new();
pub(crate) static NATIVE_FOREGROUND_BODY: Metric = Metric::new();
pub(crate) static NATIVE_PRESSURE_WAIT: Metric = Metric::new();
pub(crate) static NATIVE_FLUSH_TOTAL: Metric = Metric::new();
pub(crate) static NATIVE_DELALLOC: Metric = Metric::new();
pub(crate) static NATIVE_DIRTY_SCAN: Metric = Metric::new();
pub(crate) static NATIVE_DATA_SUBMIT: Metric = Metric::new();
pub(crate) static NATIVE_COMPLETION_WAIT: Metric = Metric::new();
pub(crate) static NATIVE_METADATA_FINISH: Metric = Metric::new();

pub(crate) static NV_FOREGROUND_TOTAL: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_PERSIST: Metric = Metric::new();
pub(crate) static NV_CAPACITY_WAIT: Metric = Metric::new();
pub(crate) static NV_FLUSH_TOTAL: Metric = Metric::new();
pub(crate) static NV_CANDIDATE_SELECT: Metric = Metric::new();
pub(crate) static NV_TARGET_PREPARE: Metric = Metric::new();
pub(crate) static NV_DATA_SUBMIT: Metric = Metric::new();
pub(crate) static NV_METADATA_SUBMIT: Metric = Metric::new();
pub(crate) static NV_ASYNC_READY_WAIT: Metric = Metric::new();
pub(crate) static NV_COMPLETION_QUEUE_WAIT: Metric = Metric::new();
pub(crate) static NV_COMPLETION_TOTAL: Metric = Metric::new();
pub(crate) static NV_DEVICE_FLUSH: Metric = Metric::new();
pub(crate) static NV_EXTENT_PUBLISH: Metric = Metric::new();
pub(crate) static NV_EXTENT_GROUP: Metric = Metric::new();
pub(crate) static NV_EXTENT_PREPARE: Metric = Metric::new();
pub(crate) static NV_EXTENT_PUBLICATION_LOCK: Metric = Metric::new();
pub(crate) static NV_EXTENT_MEMBER_VALIDATE: Metric = Metric::new();
pub(crate) static NV_EXTENT_REVERSE_LOOKUP: Metric = Metric::new();
pub(crate) static NV_EXTENT_LEAF_LOCK: Metric = Metric::new();
pub(crate) static NV_EXTENT_LEAF_READ: Metric = Metric::new();
pub(crate) static NV_EXTENT_LEAF_VALIDATE: Metric = Metric::new();
pub(crate) static NV_EXTENT_TARGET_PERSIST: Metric = Metric::new();
pub(crate) static NV_EXTENT_CREDIT_SCAN: Metric = Metric::new();
pub(crate) static NV_EXTENT_CREDIT_PERSIST: Metric = Metric::new();
pub(crate) static NV_EXTENT_OUTCOME_BUILD: Metric = Metric::new();
pub(crate) static NV_EXTENT_LEDGER: Metric = Metric::new();
pub(crate) static NV_EXTENT_RESULT: Metric = Metric::new();
pub(crate) static NV_RECLAIM: Metric = Metric::new();
pub(crate) static NV_RECLAIM_READY_WAIT: Metric = Metric::new();
pub(crate) static NV_RECLAIM_HELP: Metric = Metric::new();

pub(crate) fn dump_snapshot() {
    if !ENABLED
        || (NATIVE_FOREGROUND_TOTAL.count.load(Ordering::Relaxed) == 0
            && NV_FOREGROUND_TOTAL.count.load(Ordering::Relaxed) == 0)
    {
        return;
    }
    for (name, metric) in [
        ("native_foreground_total", &NATIVE_FOREGROUND_TOTAL),
        ("native_foreground_body", &NATIVE_FOREGROUND_BODY),
        ("native_pressure_wait", &NATIVE_PRESSURE_WAIT),
        ("native_flush_total", &NATIVE_FLUSH_TOTAL),
        ("native_delalloc", &NATIVE_DELALLOC),
        ("native_dirty_scan", &NATIVE_DIRTY_SCAN),
        ("native_data_submit", &NATIVE_DATA_SUBMIT),
        ("native_completion_wait", &NATIVE_COMPLETION_WAIT),
        ("native_metadata_finish", &NATIVE_METADATA_FINISH),
        ("nv_foreground_total", &NV_FOREGROUND_TOTAL),
        ("nv_foreground_persist", &NV_FOREGROUND_PERSIST),
        ("nv_capacity_wait", &NV_CAPACITY_WAIT),
        ("nv_flush_total", &NV_FLUSH_TOTAL),
        ("nv_candidate_select", &NV_CANDIDATE_SELECT),
        ("nv_target_prepare", &NV_TARGET_PREPARE),
        ("nv_data_submit", &NV_DATA_SUBMIT),
        ("nv_metadata_submit", &NV_METADATA_SUBMIT),
        ("nv_async_ready_wait", &NV_ASYNC_READY_WAIT),
        ("nv_completion_queue_wait", &NV_COMPLETION_QUEUE_WAIT),
        ("nv_completion_total", &NV_COMPLETION_TOTAL),
        ("nv_device_flush", &NV_DEVICE_FLUSH),
        ("nv_extent_publish", &NV_EXTENT_PUBLISH),
        ("nv_extent_group", &NV_EXTENT_GROUP),
        ("nv_extent_prepare", &NV_EXTENT_PREPARE),
        ("nv_extent_publication_lock", &NV_EXTENT_PUBLICATION_LOCK),
        ("nv_extent_member_validate", &NV_EXTENT_MEMBER_VALIDATE),
        ("nv_extent_reverse_lookup", &NV_EXTENT_REVERSE_LOOKUP),
        ("nv_extent_leaf_lock", &NV_EXTENT_LEAF_LOCK),
        ("nv_extent_leaf_read", &NV_EXTENT_LEAF_READ),
        ("nv_extent_leaf_validate", &NV_EXTENT_LEAF_VALIDATE),
        ("nv_extent_target_persist", &NV_EXTENT_TARGET_PERSIST),
        ("nv_extent_credit_scan", &NV_EXTENT_CREDIT_SCAN),
        ("nv_extent_credit_persist", &NV_EXTENT_CREDIT_PERSIST),
        ("nv_extent_outcome_build", &NV_EXTENT_OUTCOME_BUILD),
        ("nv_extent_ledger", &NV_EXTENT_LEDGER),
        ("nv_extent_result", &NV_EXTENT_RESULT),
        ("nv_reclaim", &NV_RECLAIM),
        ("nv_reclaim_ready_wait", &NV_RECLAIM_READY_WAIT),
        ("nv_reclaim_help", &NV_RECLAIM_HELP),
    ] {
        let mut histogram = String::new();
        histogram.push('[');
        for (index, bucket) in metric.histogram.iter().enumerate() {
            if index != 0 {
                histogram.push(',');
            }
            let _ = write!(histogram, "{}", bucket.load(Ordering::Relaxed));
        }
        histogram.push(']');
        ostd::error!(
            "[PIPELINE] name={} count={} total_ns={} max_ns={} units={} bins={}",
            name,
            metric.count.load(Ordering::Relaxed),
            metric.total_ns.load(Ordering::Relaxed),
            metric.max_ns.load(Ordering::Relaxed),
            metric.units.load(Ordering::Relaxed),
            histogram
        );
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn pipeline_histogram_uses_power_of_two_upper_bounds() {
        assert_eq!(histogram_bucket(0), 0);
        assert_eq!(histogram_bucket(1), 0);
        assert_eq!(histogram_bucket(2), 1);
        assert_eq!(histogram_bucket(3), 2);
        assert_eq!(histogram_bucket(4), 2);
        assert_eq!(histogram_bucket(u64::MAX), HISTOGRAM_BINS - 1);
    }
}
