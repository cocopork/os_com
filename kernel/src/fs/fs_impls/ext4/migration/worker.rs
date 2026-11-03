// SPDX-License-Identifier: MPL-2.0

//! Runtime-only queues owned by one rTIO flusher.

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::sync::LocalIrqDisabled;

use super::{CompletionJob, batch::MigrationBatch, durability::DurabilityArbiter};
use crate::prelude::*;

pub(super) struct DurableBatchResult {
    pub(super) batch: Arc<MigrationBatch>,
    pub(super) jobs: Vec<CompletionJob>,
    pub(super) durable: bool,
}

pub(crate) struct RtioWorkerRuntime {
    worker_id: usize,
    trigger: Arc<ostd::sync::WaitQueue>,
    data_complete: SpinLock<VecDeque<Arc<MigrationBatch>>, LocalIrqDisabled>,
    durable_results: SpinLock<VecDeque<DurableBatchResult>, LocalIrqDisabled>,
    data_complete_batches: AtomicUsize,
    durable_result_batches: AtomicUsize,
}

impl RtioWorkerRuntime {
    pub(crate) fn new(worker_id: usize, trigger: Arc<ostd::sync::WaitQueue>) -> Arc<Self> {
        Arc::new(Self {
            worker_id,
            trigger,
            data_complete: SpinLock::new(VecDeque::new()),
            durable_results: SpinLock::new(VecDeque::new()),
            data_complete_batches: AtomicUsize::new(0),
            durable_result_batches: AtomicUsize::new(0),
        })
    }

    pub(super) const fn worker_id(&self) -> usize {
        self.worker_id
    }

    pub(super) fn publish_data_complete(
        &self,
        batch: Arc<MigrationBatch>,
        arbiter: &DurabilityArbiter,
    ) {
        debug_assert_eq!(batch.owner_worker_id(), Some(self.worker_id));
        {
            let mut queue = self.data_complete.lock();
            queue.push_back(batch);
            self.data_complete_batches.fetch_add(1, Ordering::Release);
            arbiter.note_ready_batch();
        }
        self.trigger.wake_one();
    }

    pub(super) fn take_data_complete(&self) -> VecDeque<Arc<MigrationBatch>> {
        let mut drained = VecDeque::new();
        core::mem::swap(&mut *self.data_complete.lock(), &mut drained);
        self.data_complete_batches
            .fetch_sub(drained.len(), Ordering::AcqRel);
        drained
    }

    pub(super) fn publish_durable_result(&self, result: DurableBatchResult) {
        debug_assert_eq!(result.batch.owner_worker_id(), Some(self.worker_id));
        {
            let mut queue = self.durable_results.lock();
            queue.push_back(result);
            self.durable_result_batches.fetch_add(1, Ordering::Release);
        }
        self.trigger.wake_one();
    }

    pub(super) fn take_durable_results(&self) -> VecDeque<DurableBatchResult> {
        let mut drained = VecDeque::new();
        core::mem::swap(&mut *self.durable_results.lock(), &mut drained);
        self.durable_result_batches
            .fetch_sub(drained.len(), Ordering::AcqRel);
        drained
    }

    pub(crate) fn data_complete_depth(&self) -> usize {
        self.data_complete_batches.load(Ordering::Acquire)
    }

    pub(super) fn durable_result_depth(&self) -> usize {
        self.durable_result_batches.load(Ordering::Acquire)
    }

    pub(super) fn wake(&self) {
        self.trigger.wake_one();
    }
}
