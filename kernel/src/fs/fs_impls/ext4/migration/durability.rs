// SPDX-License-Identifier: MPL-2.0

//! Device-wide durability epochs advanced by whichever rTIO worker wins leadership.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aster_block::bio::BioStatus;

use super::{super::pipeline_profile as pp, batch::MigrationBatch, worker::DurableBatchResult};
use crate::{
    fs::{
        fs_impls::ext4::fs::Ext4,
        rnvpc::batch_metrics::{self, Phase},
    },
    prelude::*,
};

pub(crate) struct DurabilityArbiter {
    leader: AtomicBool,
    ready_batches: AtomicUsize,
    pending_data_pages: AtomicUsize,
    ready_pages: AtomicUsize,
    generation: AtomicUsize,
}

impl DurabilityArbiter {
    pub(crate) const fn new() -> Self {
        Self {
            leader: AtomicBool::new(false),
            ready_batches: AtomicUsize::new(0),
            pending_data_pages: AtomicUsize::new(0),
            ready_pages: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
        }
    }

    pub(super) fn note_submitted_pages(&self, pages: usize) {
        debug_assert_ne!(pages, 0);
        self.pending_data_pages.fetch_add(pages, Ordering::AcqRel);
    }

    pub(super) fn note_ready_batch(&self, pages: usize) {
        debug_assert_ne!(pages, 0);
        // Publish ready credit before removing pending credit. A concurrent
        // worker may defer once, but it must not mistake this transfer for an
        // empty tail and close the preceding epoch early.
        self.ready_pages.fetch_add(pages, Ordering::AcqRel);
        let previous = self.pending_data_pages.fetch_sub(pages, Ordering::AcqRel);
        debug_assert!(previous >= pages);
        self.ready_batches.fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn ready_batch_count(&self) -> usize {
        self.ready_batches.load(Ordering::Acquire)
    }

    pub(crate) fn pending_data_page_count(&self) -> usize {
        self.pending_data_pages.load(Ordering::Acquire)
    }

    pub(crate) fn ready_page_count(&self) -> usize {
        self.ready_pages.load(Ordering::Acquire)
    }

    pub(super) fn generation(&self) -> usize {
        self.generation.load(Ordering::Acquire)
    }

    pub(super) fn try_advance_background(
        &self,
        fs: &Ext4,
        contender: Option<usize>,
    ) -> Result<usize> {
        let ready_pages = self.ready_page_count();
        if ready_pages == 0
            || (ready_pages < super::MIGRATION_BATCH_PAGES && self.pending_data_page_count() != 0)
        {
            return Ok(0);
        }
        self.try_advance(fs, contender)
    }

    pub(super) fn try_advance(&self, fs: &Ext4, contender: Option<usize>) -> Result<usize> {
        if self.ready_batch_count() == 0 {
            return Ok(0);
        }
        let won = self
            .leader
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if let Some(worker_id) = contender {
            fs.rtio_profile.record_leader_attempt(worker_id, won);
        }
        if !won {
            return Ok(0);
        }

        let result = self.advance_as_leader(fs);
        self.leader.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if self.ready_batch_count() != 0 {
            for worker in fs.rtio_worker_runtimes.iter() {
                if worker.data_complete_depth() != 0 {
                    worker.wake();
                    break;
                }
            }
        }
        result
    }

    fn advance_as_leader(&self, fs: &Ext4) -> Result<usize> {
        let mut batches = Vec::new();
        for worker in fs.rtio_worker_runtimes.iter() {
            batches.extend(worker.take_data_complete());
        }
        if batches.is_empty() {
            return Ok(0);
        }
        let ready_pages = batches.iter().map(|batch| batch.page_count()).sum();
        self.ready_batches
            .fetch_sub(batches.len(), Ordering::AcqRel);
        let previous = self.ready_pages.fetch_sub(ready_pages, Ordering::AcqRel);
        debug_assert!(previous >= ready_pages);

        let epoch_batches = batches.len();
        let mut successful = Vec::new();
        let mut failed = Vec::new();
        let mut successful_pages = 0usize;
        for batch in batches {
            let jobs = batch.take_jobs();
            if jobs.iter().any(|job| job.status != BioStatus::Complete) {
                failed.push((batch, jobs));
            } else {
                successful_pages = successful_pages.saturating_add(batch.page_count());
                successful.push((batch, jobs));
            }
        }
        pp::record_rtio_epoch(epoch_batches, successful_pages, !successful.is_empty());

        let mut flush_failed = false;
        if !successful.is_empty() {
            let started = batch_metrics::now();
            let device_flush_started = pp::now();
            flush_failed = !matches!(fs.block_device().sync(), Ok(BioStatus::Complete));
            pp::NV_DEVICE_FLUSH.record_since(device_flush_started, successful_pages);
            batch_metrics::elapsed(Phase::DataBarrier, started);
        }

        let processed_pages = successful
            .iter()
            .chain(failed.iter())
            .map(|(batch, _)| batch.page_count())
            .sum();
        for (batch, jobs) in successful {
            self.publish_result(fs, batch, jobs, !flush_failed)?;
        }
        for (batch, jobs) in failed {
            self.publish_result(fs, batch, jobs, false)?;
        }
        if flush_failed {
            return_errno_with_message!(Errno::EIO, "rTIO durability FLUSH failed");
        }
        Ok(processed_pages)
    }

    fn publish_result(
        &self,
        fs: &Ext4,
        batch: Arc<MigrationBatch>,
        jobs: Vec<super::CompletionJob>,
        durable: bool,
    ) -> Result<()> {
        let owner = batch
            .owner_worker_id()
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "rTIO batch lost owner"))?;
        let runtime = fs
            .rtio_worker_runtimes
            .get(owner)
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "rTIO owner is out of range"))?;
        runtime.publish_durable_result(DurableBatchResult {
            batch,
            jobs,
            durable,
        });
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn submitted_pages_move_from_pending_to_ready_once() {
        let arbiter = DurabilityArbiter::new();

        arbiter.note_submitted_pages(7);
        assert_eq!(arbiter.pending_data_page_count(), 7);
        assert_eq!(arbiter.ready_page_count(), 0);

        arbiter.note_ready_batch(7);
        assert_eq!(arbiter.pending_data_page_count(), 0);
        assert_eq!(arbiter.ready_page_count(), 7);
        assert_eq!(arbiter.ready_batch_count(), 1);
    }
}
