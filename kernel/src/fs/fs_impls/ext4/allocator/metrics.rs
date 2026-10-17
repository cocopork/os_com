// SPDX-License-Identifier: MPL-2.0

//! Compile-time optional diagnostics; disabled in throughput comparisons.
use core::sync::atomic::{AtomicU64, Ordering};

pub(in crate::fs::fs_impls::ext4) const ENABLED: bool = false;

pub(in crate::fs::fs_impls::ext4) struct Metric {
    count: AtomicU64,
    total: AtomicU64,
    max: AtomicU64,
}
impl Metric {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }
    pub(in crate::fs::fs_impls::ext4) fn record(&self, value: u64) {
        if !ENABLED {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(value, Ordering::Relaxed);
        self.max.fetch_max(value, Ordering::Relaxed);
    }
}

pub(in crate::fs::fs_impls::ext4) struct Span {
    metric: &'static Metric,
    start: u64,
}
impl Span {
    pub(in crate::fs::fs_impls::ext4) fn new(metric: &'static Metric) -> Self {
        Self {
            metric,
            start: if ENABLED {
                aster_time::read_monotonic_time().as_nanos() as u64
            } else {
                0
            },
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if ENABLED {
            self.metric.record(
                (aster_time::read_monotonic_time().as_nanos() as u64).saturating_sub(self.start),
            );
        }
    }
}

pub(in crate::fs::fs_impls::ext4) static CAPACITY_WAIT: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static CAPACITY_HOLD: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static GROUP_WAIT: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static GROUP_HOLD: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static PUBLISH_WAIT: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static PUBLISH_HOLD: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static GROUP_VISITS: Metric = Metric::new();
pub(in crate::fs::fs_impls::ext4) static PINNED_DENIALS: Metric = Metric::new();

pub(in crate::fs::fs_impls::ext4) fn dump() {
    if !ENABLED {
        return;
    }
    for (name, metric) in [
        ("capacity_wait", &CAPACITY_WAIT),
        ("capacity_hold", &CAPACITY_HOLD),
        ("group_wait", &GROUP_WAIT),
        ("group_hold", &GROUP_HOLD),
        ("publish_wait", &PUBLISH_WAIT),
        ("publish_hold", &PUBLISH_HOLD),
        ("group_visits", &GROUP_VISITS),
        ("pinned_denials", &PINNED_DENIALS),
    ] {
        ostd::info!(
            "EXT4_ALLOC_METRIC name={} count={} total={} max={}",
            name,
            metric.count.load(Ordering::Relaxed),
            metric.total.load(Ordering::Relaxed),
            metric.max.load(Ordering::Relaxed)
        );
    }
}
