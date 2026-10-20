// SPDX-License-Identifier: MPL-2.0

//! Runtime-only completion gate for one orderless migration round.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ostd::sync::LocalIrqDisabled;

use super::{CompletionJob, CompletionQueue};
use crate::{fs::fs_impls::ext4::pipeline_profile as pp, prelude::*};

pub(crate) struct MigrationBatch {
    remaining_data: AtomicUsize,
    metadata_ready: AtomicBool,
    metadata_failed: AtomicBool,
    queued: AtomicBool,
    page_count: usize,
    created_at_ns: u64,
    ready_at_ns: AtomicU64,
    jobs: SpinLock<Vec<CompletionJob>, LocalIrqDisabled>,
}

impl MigrationBatch {
    pub(super) fn new(data_requests: usize, page_count: usize) -> Arc<Self> {
        debug_assert!(data_requests != 0);
        Arc::new(Self {
            remaining_data: AtomicUsize::new(data_requests),
            metadata_ready: AtomicBool::new(false),
            metadata_failed: AtomicBool::new(false),
            queued: AtomicBool::new(false),
            page_count,
            created_at_ns: pp::now(),
            ready_at_ns: AtomicU64::new(0),
            jobs: SpinLock::new(Vec::with_capacity(data_requests)),
        })
    }

    pub(super) const fn page_count(&self) -> usize {
        self.page_count
    }

    pub(super) fn completion_started(&self) -> u64 {
        let started = pp::now();
        let ready = self.ready_at_ns.load(Ordering::Acquire);
        if ready != 0 {
            pp::NV_COMPLETION_QUEUE_WAIT.record_since(ready, self.page_count);
        }
        started
    }

    pub(super) fn complete_data(
        self: &Arc<Self>,
        job: CompletionJob,
        queue: &CompletionQueue,
        trigger: &ostd::sync::WaitQueue,
    ) {
        self.jobs.lock().push(job);
        let previous = self.remaining_data.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0);
        if previous == 1 {
            self.try_queue(queue, trigger);
        }
    }

    pub(super) fn complete_metadata(
        self: &Arc<Self>,
        success: bool,
        queue: &CompletionQueue,
        trigger: &ostd::sync::WaitQueue,
    ) {
        if self.metadata_ready.swap(true, Ordering::AcqRel) {
            return;
        }
        if !success {
            self.metadata_failed.store(true, Ordering::Release);
        }
        self.try_queue(queue, trigger);
    }

    fn try_queue(self: &Arc<Self>, queue: &CompletionQueue, trigger: &ostd::sync::WaitQueue) {
        if !self.metadata_ready.load(Ordering::Acquire)
            || self.remaining_data.load(Ordering::Acquire) != 0
            || self.queued.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let ready_at = pp::now();
        pp::NV_ASYNC_READY_WAIT.record_since(self.created_at_ns, self.page_count);
        self.ready_at_ns.store(ready_at, Ordering::Release);
        queue.lock().push_back(self.clone());
        trigger.wake_one();
    }

    pub(super) fn take_jobs(&self) -> (Vec<CompletionJob>, bool) {
        let mut jobs = self.jobs.lock();
        let completed = core::mem::take(&mut *jobs);
        (completed, self.metadata_failed.load(Ordering::Acquire))
    }
}

#[cfg(ktest)]
mod tests {
    use aster_block::bio::BioStatus;
    use ostd::prelude::ktest;

    use super::*;

    fn queue() -> CompletionQueue {
        Arc::new(SpinLock::new(VecDeque::new()))
    }

    #[ktest]
    fn orderless_batch_waits_for_data_and_metadata_once() {
        let trigger = ostd::sync::WaitQueue::new();
        let completed = queue();
        let batch = MigrationBatch::new(1, 7);
        batch.complete_data(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &completed,
            &trigger,
        );
        assert!(completed.lock().is_empty());
        batch.complete_metadata(true, &completed, &trigger);
        batch.complete_metadata(true, &completed, &trigger);
        assert_eq!(completed.lock().len(), 1);
        let ready = completed.lock().pop_front().unwrap();
        assert_eq!(ready.page_count(), 7);
        let (jobs, failed) = ready.take_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(!failed);

        let completed = queue();
        let failed = MigrationBatch::new(1, 1);
        failed.complete_metadata(false, &completed, &trigger);
        assert!(completed.lock().is_empty());
        failed.complete_data(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &completed,
            &trigger,
        );
        let failed = completed.lock().pop_front().unwrap();
        assert!(failed.take_jobs().1);
    }
}
