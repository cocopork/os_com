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
pub(crate) static NV_FOREGROUND_SSD_ALLOCATE: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_GROUP_RESERVE: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_BLOCK_ALLOCATE: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_ALLOCATION_METADATA: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_ALLOCATION_RECORD_PUBLISH: Metric = Metric::new();
pub(crate) static NV_FOREGROUND_ALLOCATION_RECORD_RECLAIM: Metric = Metric::new();
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

struct RtioMultiWorkerMetrics {
    configured_workers: AtomicU64,
    active_workers: AtomicU64,
    peak_workers: AtomicU64,
    epochs: AtomicU64,
    epoch_batches: AtomicU64,
    epoch_pages: AtomicU64,
    device_flushes: AtomicU64,
    submitted_pages: AtomicU64,
    submitted_bios: AtomicU64,
    max_in_flight_pages: AtomicU64,
}

impl RtioMultiWorkerMetrics {
    const fn new() -> Self {
        Self {
            configured_workers: AtomicU64::new(0),
            active_workers: AtomicU64::new(0),
            peak_workers: AtomicU64::new(0),
            epochs: AtomicU64::new(0),
            epoch_batches: AtomicU64::new(0),
            epoch_pages: AtomicU64::new(0),
            device_flushes: AtomicU64::new(0),
            submitted_pages: AtomicU64::new(0),
            submitted_bios: AtomicU64::new(0),
            max_in_flight_pages: AtomicU64::new(0),
        }
    }
}

static RTIO_MULTI_WORKER: RtioMultiWorkerMetrics = RtioMultiWorkerMetrics::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RtioMultiWorkerSnapshot {
    pub(crate) configured_workers: u64,
    pub(crate) active_workers: u64,
    pub(crate) peak_workers: u64,
    pub(crate) epochs: u64,
    pub(crate) epoch_batches: u64,
    pub(crate) epoch_pages: u64,
    pub(crate) device_flushes: u64,
    pub(crate) submitted_pages: u64,
    pub(crate) submitted_bios: u64,
    pub(crate) max_in_flight_pages: u64,
}

const fn rtio_metrics_enabled() -> bool {
    ENABLED || cfg!(ktest)
}

pub(crate) fn configure_rtio_workers(workers: usize) {
    if rtio_metrics_enabled() {
        RTIO_MULTI_WORKER
            .configured_workers
            .store(workers as u64, Ordering::Relaxed);
    }
}

pub(crate) struct RtioWorkerRound {
    recorded: bool,
}

pub(crate) fn enter_rtio_worker_round() -> RtioWorkerRound {
    let recorded = rtio_metrics_enabled();
    if recorded {
        let active = RTIO_MULTI_WORKER
            .active_workers
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        RTIO_MULTI_WORKER
            .peak_workers
            .fetch_max(active, Ordering::Relaxed);
    }
    RtioWorkerRound { recorded }
}

impl Drop for RtioWorkerRound {
    fn drop(&mut self) {
        if self.recorded {
            RTIO_MULTI_WORKER
                .active_workers
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub(crate) fn record_rtio_submission(pages: usize, bios: usize, in_flight_pages: usize) {
    if !rtio_metrics_enabled() {
        return;
    }
    RTIO_MULTI_WORKER
        .submitted_pages
        .fetch_add(pages as u64, Ordering::Relaxed);
    RTIO_MULTI_WORKER
        .submitted_bios
        .fetch_add(bios as u64, Ordering::Relaxed);
    RTIO_MULTI_WORKER
        .max_in_flight_pages
        .fetch_max(in_flight_pages as u64, Ordering::Relaxed);
}

pub(crate) fn record_rtio_epoch(batches: usize, pages: usize, device_flush: bool) {
    if !rtio_metrics_enabled() {
        return;
    }
    RTIO_MULTI_WORKER.epochs.fetch_add(1, Ordering::Relaxed);
    RTIO_MULTI_WORKER
        .epoch_batches
        .fetch_add(batches as u64, Ordering::Relaxed);
    RTIO_MULTI_WORKER
        .epoch_pages
        .fetch_add(pages as u64, Ordering::Relaxed);
    if device_flush {
        RTIO_MULTI_WORKER
            .device_flushes
            .fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn rtio_multi_worker_snapshot() -> RtioMultiWorkerSnapshot {
    RtioMultiWorkerSnapshot {
        configured_workers: RTIO_MULTI_WORKER.configured_workers.load(Ordering::Relaxed),
        active_workers: RTIO_MULTI_WORKER.active_workers.load(Ordering::Relaxed),
        peak_workers: RTIO_MULTI_WORKER.peak_workers.load(Ordering::Relaxed),
        epochs: RTIO_MULTI_WORKER.epochs.load(Ordering::Relaxed),
        epoch_batches: RTIO_MULTI_WORKER.epoch_batches.load(Ordering::Relaxed),
        epoch_pages: RTIO_MULTI_WORKER.epoch_pages.load(Ordering::Relaxed),
        device_flushes: RTIO_MULTI_WORKER.device_flushes.load(Ordering::Relaxed),
        submitted_pages: RTIO_MULTI_WORKER.submitted_pages.load(Ordering::Relaxed),
        submitted_bios: RTIO_MULTI_WORKER.submitted_bios.load(Ordering::Relaxed),
        max_in_flight_pages: RTIO_MULTI_WORKER
            .max_in_flight_pages
            .load(Ordering::Relaxed),
    }
}

#[cfg(ktest)]
fn reset_rtio_multi_worker_for_test() {
    for counter in [
        &RTIO_MULTI_WORKER.configured_workers,
        &RTIO_MULTI_WORKER.active_workers,
        &RTIO_MULTI_WORKER.peak_workers,
        &RTIO_MULTI_WORKER.epochs,
        &RTIO_MULTI_WORKER.epoch_batches,
        &RTIO_MULTI_WORKER.epoch_pages,
        &RTIO_MULTI_WORKER.device_flushes,
        &RTIO_MULTI_WORKER.submitted_pages,
        &RTIO_MULTI_WORKER.submitted_bios,
        &RTIO_MULTI_WORKER.max_in_flight_pages,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Mount-local worker bindings and accepted migration data submissions.
pub(super) struct RtioQueueProfile {
    workers: usize,
    submissions: Vec<RtioQueueSubmissions>,
}

struct RtioQueueSubmissions {
    pages: AtomicU64,
    bios: AtomicU64,
    leader_attempts: AtomicU64,
    leader_wins: AtomicU64,
    completion_batches: AtomicU64,
    completion_pages: AtomicU64,
    max_data_complete_depth: AtomicU64,
    max_durable_result_depth: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RtioQueueSubmissionSnapshot {
    pub(super) pages: u64,
    pub(super) bios: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RtioOwnerSnapshot {
    pub(super) leader_attempts: u64,
    pub(super) leader_wins: u64,
    pub(super) completion_batches: u64,
    pub(super) completion_pages: u64,
    pub(super) max_data_complete_depth: u64,
    pub(super) max_durable_result_depth: u64,
}

impl RtioQueueProfile {
    pub(super) fn new(workers: usize, submission_queues: u16) -> Self {
        Self {
            workers,
            submissions: (0..submission_queues)
                .map(|_| RtioQueueSubmissions {
                    pages: AtomicU64::new(0),
                    bios: AtomicU64::new(0),
                    leader_attempts: AtomicU64::new(0),
                    leader_wins: AtomicU64::new(0),
                    completion_batches: AtomicU64::new(0),
                    completion_pages: AtomicU64::new(0),
                    max_data_complete_depth: AtomicU64::new(0),
                    max_durable_result_depth: AtomicU64::new(0),
                })
                .collect(),
        }
    }

    pub(super) fn worker_count(&self) -> usize {
        self.workers
    }

    pub(super) fn record_worker_mapping(
        &self,
        device: &dyn aster_block::BlockDevice,
        worker_id: usize,
    ) {
        if ENABLED {
            ostd::error!(
                "[PIPELINE] name=rtio_worker_mapping device={} worker={} queue={}",
                device.name(),
                worker_id,
                worker_id
            );
        }
    }

    pub(super) fn record_submission(&self, queue_id: u16, pages: usize, bios: usize) {
        if !rtio_metrics_enabled() {
            return;
        }
        let submission = &self.submissions[usize::from(queue_id)];
        submission.pages.fetch_add(pages as u64, Ordering::Relaxed);
        submission.bios.fetch_add(bios as u64, Ordering::Relaxed);
    }

    pub(super) fn submission_snapshot(&self, queue_id: u16) -> Option<RtioQueueSubmissionSnapshot> {
        let submission = self.submissions.get(usize::from(queue_id))?;
        Some(RtioQueueSubmissionSnapshot {
            pages: submission.pages.load(Ordering::Relaxed),
            bios: submission.bios.load(Ordering::Relaxed),
        })
    }

    pub(super) fn record_leader_attempt(&self, worker_id: usize, won: bool) {
        if !rtio_metrics_enabled() {
            return;
        }
        let Some(worker) = self.submissions.get(worker_id) else {
            return;
        };
        worker.leader_attempts.fetch_add(1, Ordering::Relaxed);
        if won {
            worker.leader_wins.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn record_owner_completion(
        &self,
        worker_id: usize,
        pages: usize,
        batches: usize,
        data_complete_depth: usize,
        durable_result_depth: usize,
    ) {
        if !rtio_metrics_enabled() {
            return;
        }
        let Some(worker) = self.submissions.get(worker_id) else {
            return;
        };
        worker
            .completion_batches
            .fetch_add(batches as u64, Ordering::Relaxed);
        worker
            .completion_pages
            .fetch_add(pages as u64, Ordering::Relaxed);
        worker
            .max_data_complete_depth
            .fetch_max(data_complete_depth as u64, Ordering::Relaxed);
        worker
            .max_durable_result_depth
            .fetch_max(durable_result_depth as u64, Ordering::Relaxed);
    }

    pub(super) fn owner_snapshot(&self, worker_id: usize) -> Option<RtioOwnerSnapshot> {
        let worker = self.submissions.get(worker_id)?;
        Some(RtioOwnerSnapshot {
            leader_attempts: worker.leader_attempts.load(Ordering::Relaxed),
            leader_wins: worker.leader_wins.load(Ordering::Relaxed),
            completion_batches: worker.completion_batches.load(Ordering::Relaxed),
            completion_pages: worker.completion_pages.load(Ordering::Relaxed),
            max_data_complete_depth: worker.max_data_complete_depth.load(Ordering::Relaxed),
            max_durable_result_depth: worker.max_durable_result_depth.load(Ordering::Relaxed),
        })
    }

    fn dump_snapshot(
        &self,
        device: &dyn aster_block::BlockDevice,
        fs_id: u64,
        runtimes: &[Arc<super::migration::worker::RtioWorkerRuntime>],
    ) {
        for worker_id in 0..self.workers {
            let submission = self.submission_snapshot(worker_id as u16).unwrap();
            let owner = self.owner_snapshot(worker_id).unwrap();
            let load = crate::fs::rnvpc::pool()
                .and_then(|pool| pool.migration_worker_load_snapshot(fs_id, worker_id))
                .unwrap_or_default();
            let (data_complete_depth, durable_result_depth) = runtimes
                .get(worker_id)
                .map(|runtime| {
                    (
                        runtime.data_complete_depth(),
                        runtime.durable_result_depth(),
                    )
                })
                .unwrap_or_default();
            ostd::error!(
                "[PIPELINE] name=rtio_worker device={} worker={} queue={} submitted_pages={} submitted_bios={} leader_attempts={} leader_wins={} completion_batches={} completion_pages={} max_data_complete_depth={} max_durable_result_depth={} data_complete_depth={} durable_result_depth={} owned_inodes={} queued_dirty_pages={} in_flight_pages={} completion_pending_pages={} ready_inode_dequeues={}",
                device.name(),
                worker_id,
                worker_id,
                submission.pages,
                submission.bios,
                owner.leader_attempts,
                owner.leader_wins,
                owner.completion_batches,
                owner.completion_pages,
                owner.max_data_complete_depth,
                owner.max_durable_result_depth,
                data_complete_depth,
                durable_result_depth,
                load.owned_inodes,
                load.queued_dirty_pages,
                load.in_flight_pages,
                load.completion_pending_pages,
                load.ready_inode_dequeues,
            );
        }
        // Queue snapshots are taken only at the existing report boundary.
        for queue_id in 0..device.metadata().nr_submission_queues {
            let Some(queue) = device.queue_diagnostics(queue_id) else {
                ostd::error!(
                    "[PIPELINE] name=block_submission_queue device={} queue={} supported=false",
                    device.name(),
                    queue_id
                );
                continue;
            };
            ostd::error!(
                "[PIPELINE] name=block_submission_queue device={} queue={} supported=true queued_bios={} queued_pages={} submitted_bios={} submitted_pages={} staging_depth={} max_sampled_staging_depth={} descriptor_wait_count={} descriptor_wait_ticks={} active_forwarders={} peak_forwarders={} irq_completions={} in_flight_requests={}",
                device.name(),
                queue_id,
                queue.queued_bios,
                queue.queued_pages,
                queue.submitted_bios,
                queue.submitted_pages,
                queue.staging_depth,
                queue.max_sampled_staging_depth,
                queue.descriptor_wait_count,
                queue.descriptor_wait_ticks,
                queue.active_forwarders,
                queue.peak_forwarders,
                queue.irq_completions,
                queue.in_flight_requests,
            );
        }
    }
}

pub(super) fn dump_snapshot(
    device: &dyn aster_block::BlockDevice,
    queues: &RtioQueueProfile,
    fs_id: u64,
    runtimes: &[Arc<super::migration::worker::RtioWorkerRuntime>],
) {
    if !ENABLED {
        return;
    }
    queues.dump_snapshot(device, fs_id, runtimes);
    if NATIVE_FOREGROUND_TOTAL.count.load(Ordering::Relaxed) == 0
        && NV_FOREGROUND_TOTAL.count.load(Ordering::Relaxed) == 0
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
        ("nv_foreground_ssd_allocate", &NV_FOREGROUND_SSD_ALLOCATE),
        ("nv_foreground_group_reserve", &NV_FOREGROUND_GROUP_RESERVE),
        (
            "nv_foreground_block_allocate",
            &NV_FOREGROUND_BLOCK_ALLOCATE,
        ),
        (
            "nv_foreground_allocation_metadata",
            &NV_FOREGROUND_ALLOCATION_METADATA,
        ),
        (
            "nv_foreground_allocation_record_publish",
            &NV_FOREGROUND_ALLOCATION_RECORD_PUBLISH,
        ),
        (
            "nv_foreground_allocation_record_reclaim",
            &NV_FOREGROUND_ALLOCATION_RECORD_RECLAIM,
        ),
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
    let rtio = rtio_multi_worker_snapshot();
    ostd::error!(
        "[PIPELINE] name=rtio_multi_worker configured={} active={} peak={} epochs={} epoch_batches={} epoch_pages={} device_flushes={} submitted_pages={} submitted_bios={} max_in_flight_pages={}",
        rtio.configured_workers,
        rtio.active_workers,
        rtio.peak_workers,
        rtio.epochs,
        rtio.epoch_batches,
        rtio.epoch_pages,
        rtio.device_flushes,
        rtio.submitted_pages,
        rtio.submitted_bios,
        rtio.max_in_flight_pages,
    );
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

    #[ktest]
    fn rtio_queue_profiles_are_mount_scoped() {
        let first = RtioQueueProfile::new(2, 4);
        let second = RtioQueueProfile::new(1, 1);
        first.record_submission(0, 4, 2);
        first.record_submission(1, 8, 3);
        first.record_submission(0, 2, 1);
        assert_eq!(first.worker_count(), 2);
        assert_eq!(
            first.submission_snapshot(0).unwrap(),
            RtioQueueSubmissionSnapshot { pages: 6, bios: 3 }
        );
        assert_eq!(
            first.submission_snapshot(1).unwrap(),
            RtioQueueSubmissionSnapshot { pages: 8, bios: 3 }
        );
        assert_eq!(
            first.submission_snapshot(2).unwrap(),
            RtioQueueSubmissionSnapshot { pages: 0, bios: 0 }
        );
        assert_eq!(first.submission_snapshot(4), None);
        assert_eq!(second.worker_count(), 1);
        assert_eq!(
            second.submission_snapshot(0).unwrap(),
            RtioQueueSubmissionSnapshot { pages: 0, bios: 0 }
        );
    }

    #[ktest]
    fn rtio_multi_worker_counter_accounting() {
        reset_rtio_multi_worker_for_test();
        configure_rtio_workers(4);
        let first = enter_rtio_worker_round();
        let second = enter_rtio_worker_round();
        record_rtio_submission(32, 1, 32);
        record_rtio_submission(64, 2, 96);
        record_rtio_epoch(2, 96, true);
        drop(second);
        drop(first);

        let snapshot = rtio_multi_worker_snapshot();
        assert_eq!(snapshot.configured_workers, 4);
        assert_eq!(snapshot.active_workers, 0);
        assert_eq!(snapshot.peak_workers, 2);
        assert_eq!(snapshot.epochs, 1);
        assert_eq!(snapshot.epoch_batches, 2);
        assert_eq!(snapshot.epoch_pages, 96);
        assert_eq!(snapshot.device_flushes, 1);
        assert_eq!(snapshot.submitted_pages, 96);
        assert_eq!(snapshot.submitted_bios, 3);
        assert_eq!(snapshot.max_in_flight_pages, 96);
    }

    #[ktest]
    fn rtio_owner_diagnostics_are_mount_scoped() {
        let profile = RtioQueueProfile::new(2, 4);
        profile.record_leader_attempt(0, true);
        profile.record_leader_attempt(1, false);
        profile.record_owner_completion(0, 32, 2, 3, 1);
        let first = profile.owner_snapshot(0).unwrap();
        let second = profile.owner_snapshot(1).unwrap();
        assert_eq!(first.leader_attempts, 1);
        assert_eq!(first.leader_wins, 1);
        assert_eq!(first.completion_batches, 2);
        assert_eq!(first.completion_pages, 32);
        assert_eq!(first.max_data_complete_depth, 3);
        assert_eq!(first.max_durable_result_depth, 1);
        assert_eq!(second.leader_attempts, 1);
        assert_eq!(second.leader_wins, 0);
        assert_eq!(second.completion_pages, 0);
    }
}
