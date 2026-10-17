// SPDX-License-Identifier: MPL-2.0

//! Two-worker, bounded asynchronous NV migration. BIO callbacks enqueue only;
//! process-context completion owns durability, publication and reclamation.
use core::sync::atomic::Ordering;

use aster_block::bio::BioDirection;
use ostd::sync::{LocalIrqDisabled, WaitQueue};

use super::{
    fs::{AllocIntent, Ext4},
    prelude::*,
};
use crate::fs::rnvpc::{self, frame::EvictingDataPage, metadata::PersistentObjectRef};

pub(super) const MAX_IN_FLIGHT: usize = 128;
pub(super) type CompletionQueue = Arc<SpinLock<VecDeque<CompletionJob>, LocalIrqDisabled>>;
pub(super) struct CompletionJob {
    status: BioStatus,
    pages: Vec<PendingPage>,
}
struct PendingPage {
    inode: u64,
    index: u64,
    page: crate::fs::rnvpc::id::NvPageId,
    generation: u64,
    target: u64,
    reference: Option<PersistentObjectRef>,
    lease: EvictingDataPage,
}

impl Ext4 {
    pub(super) fn start_nv_migration(self: &Arc<Self>) {
        if !self.nv_migration_enabled() {
            return;
        }
        let weak = Arc::downgrade(self);
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            let sleep = WaitQueue::new();
            loop {
                {
                    let Some(fs) = weak.upgrade() else {
                        break;
                    };
                    if fs.is_shutdown() {
                        break;
                    }
                    if crate::fs::rnvpc::needs_flush_watermark() {
                        if let Err(error) = fs.migrate_nv_round(64) {
                            warn!("NV migration retry pending: {error:?}");
                        }
                    }
                    if fs.nv_migration_failed.load(Ordering::Acquire) {
                        break;
                    }
                }
                let _ = sleep
                    .wait_until_or_timeout(|| -> Option<()> { None }, &Duration::from_millis(10));
            }
        })
        .spawn();
        let weak = Arc::downgrade(self);
        let trigger = self.nv_completion_trigger.clone();
        let queue = self.nv_completion_queue.clone();
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            loop {
                {
                    let Some(fs) = weak.upgrade() else {
                        break;
                    };
                    if let Err(error) = fs.process_nv_completions() {
                        warn!("NV completion retry pending: {error:?}");
                    }
                    if fs.is_shutdown() && fs.nv_in_flight.load(Ordering::Acquire) == 0 {
                        break;
                    }
                }
                let _ = trigger.wait_until_or_timeout(
                    || (!queue.lock().is_empty()).then_some(()),
                    &Duration::from_millis(10),
                );
            }
        })
        .spawn();
    }

    pub(super) fn migrate_nv_round(&self, budget: usize) -> Result<usize> {
        let _serial = self.nv_migration_lock.lock();
        if self.nv_migration_failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "ambiguous migration target requires recovery");
        }
        let budget =
            budget.min(MAX_IN_FLIGHT.saturating_sub(self.nv_in_flight.load(Ordering::Acquire)));
        if budget == 0 {
            return Ok(0);
        }
        let Some(pool) = rnvpc::pool() else {
            return Ok(0);
        };
        let mut pages = Vec::new();
        if self.rnvpc_extend_enabled() {
            for candidate in rnvpc::rnvpce_flush_candidates(self.rnvpc_fs_id(), budget) {
                let lease = match pool
                    .pin_for_writeback_seq(candidate.page, candidate.reference.allocation_seq())
                {
                    Ok(lease) => lease,
                    Err(_) => {
                        let _ = pool.enqueue_dirty_nvpage(
                            candidate.page,
                            candidate.reference.allocation_seq(),
                        );
                        continue;
                    }
                };
                let target = if candidate.target == 0 {
                    let op = self.begin_op(self.single_block_map_credits(0))?;
                    let blocks =
                        self.alloc_blocks_with_intent(1, 0, op.get(), AllocIntent::RnvpceReserved)?;
                    let target = blocks.start;
                    drop(op);
                    // Allocation durability precedes the persistent target identity.
                    if let Err(error) = self.persist_nv_allocations() {
                        // No NV descriptor names this block yet, so the ordinary
                        // EXT4 free transaction may roll back the private allocation.
                        let rollback = (|| {
                            let op = self.begin_op(self.single_block_map_credits(0))?;
                            self.free_blocks(
                                super::journal::BlockFreeAuth::without_revoke_duty(target, 1),
                                op.get(),
                            )?;
                            drop(op);
                            self.persist_nv_allocations()
                        })();
                        if let Err(rollback_error) = rollback {
                            self.nv_migration_failed.store(true, Ordering::Release);
                            warn!("NV target rollback durability failed: {rollback_error:?}");
                        }
                        return Err(error);
                    }
                    // Once target publication is attempted, a failure is ambiguous:
                    // retain the allocation rather than free a possibly durable edge.
                    if let Err(error) = lease.assign_migration_target(candidate.reference, target) {
                        // Bound leaks after ambiguous persistence: stop further
                        // allocation until recovery establishes target ownership.
                        self.nv_migration_failed.store(true, Ordering::Release);
                        return Err(error);
                    }
                    target
                } else {
                    candidate.target
                };
                pages.push(PendingPage {
                    inode: candidate.inode,
                    index: candidate.page_index,
                    page: candidate.page,
                    generation: candidate.reference.allocation_seq(),
                    target,
                    reference: Some(candidate.reference),
                    lease,
                });
            }
        } else {
            for (inode, index, page, target, generation, seq) in
                rnvpc::flush_candidates(self.rnvpc_fs_id(), budget)
            {
                if target == 0 {
                    continue;
                }
                if let Ok(lease) = pool.pin_for_writeback_seq(page, seq) {
                    pages.push(PendingPage {
                        inode,
                        index,
                        page,
                        generation,
                        target,
                        reference: None,
                        lease,
                    });
                }
            }
        }
        let submitted = pages.len();
        if submitted == 0 {
            return Ok(0);
        }
        pages.sort_by_key(|page| page.target);
        let max_segments = self
            .block_device()
            .metadata()
            .max_nr_segments_per_bio
            .max(1)
            .min(MAX_IN_FLIGHT);
        let mut pending = pages.into_iter().peekable();
        while let Some(first) = pending.next() {
            let target = first.target;
            let mut run = vec![first];
            while run.len() < max_segments
                && pending
                    .peek()
                    .is_some_and(|next| next.target == target + run.len() as u64)
            {
                run.push(pending.next().unwrap());
            }
            let mut segments = Vec::with_capacity(run.len());
            for page in &run {
                segments.push(BioSegment::new_from_segment(
                    page.lease.dma_segment()?,
                    BioDirection::ToDevice,
                ));
            }
            let count = run.len();
            let completed = self.nv_completion_queue.clone();
            let trigger = self.nv_completion_trigger.clone();
            self.nv_in_flight.fetch_add(count, Ordering::AcqRel);
            let callback: BioCompleteFn = Box::new(move |status| {
                // Queue capacity is bounded by page credits; no IRQ allocation,
                // persistence, descriptor mutation, or lease release occurs here.
                completed
                    .lock()
                    .push_back(CompletionJob { status, pages: run });
                trigger.wake_one();
            });
            let mut batch = IoBatch::new();
            if let Err(error) =
                self.write_segments_async(target, segments, Some(callback), &mut batch)
            {
                self.nv_in_flight.fetch_sub(count, Ordering::AcqRel);
                self.nv_completion_trigger.wake_all();
                return Err(error);
            }
        }
        Ok(submitted)
    }

    pub(super) fn process_nv_completions(&self) -> Result<usize> {
        let _serial = self.nv_completion_lock.lock();
        // Move the whole backing queue to process context without allocating
        // or releasing object leases while holding its IRQ-safe spinlock.
        let mut drained = VecDeque::with_capacity(MAX_IN_FLIGHT);
        core::mem::swap(&mut *self.nv_completion_queue.lock(), &mut drained);
        if drained.is_empty() {
            return Ok(0);
        }
        let count = drained.iter().map(|job| job.pages.len()).sum::<usize>();
        let result = self.finish_nv_completions(&mut drained);
        drop(drained);
        self.nv_in_flight.fetch_sub(count, Ordering::AcqRel);
        self.nv_completion_trigger.wake_all();
        result.map(|()| count)
    }

    fn finish_nv_completions(&self, jobs: &mut VecDeque<CompletionJob>) -> Result<()> {
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        self.persist_nv_allocations()?;
        let mut groups: BTreeMap<u64, Vec<PendingPage>> = BTreeMap::new();
        let mut failure = None;
        while let Some(job) = jobs.pop_front() {
            if job.status != BioStatus::Complete {
                failure = Some(Error::new(Errno::EIO));
                continue;
            }
            for page in job.pages {
                groups.entry(page.inode).or_default().push(page);
            }
        }
        for (inode_id, pages) in groups {
            if self.rnvpc_extend_enabled() {
                let updates: Vec<_> = pages
                    .iter()
                    .map(|page| rnvpc::write_transaction::SsdMigrationPage {
                        logical_block: page.index,
                        reference: page.reference.unwrap(),
                        ssd_block: page.target,
                    })
                    .collect();
                let results = rnvpc::write_transaction::migrate_mapped_nv_pages_to_ssd_by_reverse(
                    pool,
                    self.rnvpc_fs_id(),
                    inode_id,
                    &updates,
                    None,
                )?;
                for (page, result) in pages.into_iter().zip(results) {
                    let result = result.and_then(|consumed| {
                        if consumed {
                            self.release_rnvpce_blocks(1)?;
                        }
                        page.lease
                            .reclaim_after_ssd_publish(page.reference.unwrap(), None)
                    });
                    if let Err(error) = result {
                        failure = Some(error);
                    }
                }
            } else {
                for page in pages {
                    let result = (|| {
                        let inode = self.read_inode(
                            page.inode
                                .try_into()
                                .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                        )?;
                        inode.materialize_rnvpce_ssd_page(self, page.index, page.target)?;
                        self.persist_nv_allocations()?;
                        if rnvpc::prepare_flush_completion(
                            self.rnvpc_fs_id(),
                            page.inode,
                            page.index,
                            page.page,
                            page.generation,
                        )? {
                            page.lease.finish(None)?;
                            rnvpc::finalize_flush_completion(
                                self.rnvpc_fs_id(),
                                page.inode,
                                page.page,
                                page.generation,
                            )?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        failure = Some(error);
                    }
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    fn persist_nv_allocations(&self) -> Result<()> {
        if let Some(journal) = self.journal() {
            journal.commit_and_wait_running()?;
        } else {
            self.sync_metadata()?;
        }
        if self.block_device().sync()? != BioStatus::Complete {
            return_errno!(Errno::EIO);
        }
        Ok(())
    }

    pub(super) fn drain_nv_in_flight(&self) -> Result<()> {
        let mut failure = None;
        while self.nv_in_flight.load(Ordering::Acquire) != 0 {
            if let Err(error) = self.process_nv_completions() {
                failure = Some(error);
            }
            let _ = self.nv_completion_trigger.wait_until_or_timeout(
                || {
                    (self.nv_in_flight.load(Ordering::Acquire) == 0
                        || !self.nv_completion_queue.lock().is_empty())
                    .then_some(())
                },
                &Duration::from_millis(10),
            );
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn export_nv_for_sync(&self) -> Result<()> {
        if !self.rnvpc_extend_enabled() {
            return Ok(());
        }
        loop {
            let submitted = self.migrate_nv_round(MAX_IN_FLIGHT)?;
            self.drain_nv_in_flight()?;
            if submitted == 0 {
                break;
            }
        }
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        for ino in pool.fixed_inode_ids(self.rnvpc_fs_id())? {
            self.read_inode(ino.try_into().map_err(|_| Error::new(Errno::EOVERFLOW))?)?
                .export_rnvpce_extents(self)?;
        }
        self.persist_nv_allocations()
    }
}
