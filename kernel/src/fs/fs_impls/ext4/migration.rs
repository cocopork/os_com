// SPDX-License-Identifier: MPL-2.0

//! Two-worker, bounded asynchronous NV migration. BIO callbacks enqueue only;
//! process-context completion owns durability, publication and reclamation.
use core::sync::atomic::Ordering;

use aster_block::bio::BioDirection;
use ostd::sync::LocalIrqDisabled;

use super::{
    fs::{AllocIntent, Ext4},
    pipeline_profile as pp,
    prelude::*,
};
use crate::fs::rnvpc::{
    self,
    batch_metrics::{self, Event, Phase},
    frame::{EvictingDataPage, MigrationExtentIdentity},
    metadata::PersistentObjectRef,
};

mod batch;

use batch::MigrationBatch;

const MIGRATION_BATCH_PAGES: usize = super::writeback::WRITEBACK_BATCH;
pub(super) type CompletionQueue = Arc<SpinLock<VecDeque<Arc<MigrationBatch>>, LocalIrqDisabled>>;
pub(super) struct CompletionJob {
    status: BioStatus,
    pages: Vec<PendingPage>,
}
struct PendingPage {
    inode: u64,
    index: u64,
    page: rnvpc::id::NvPageId,
    generation: u64,
    target: u64,
    reference: Option<PersistentObjectRef>,
    extent: Option<Arc<MigrationExtentIdentity>>,
    lease: EvictingDataPage,
}

impl Ext4 {
    pub(super) fn start_nv_migration(self: &Arc<Self>) {
        if !self.nv_migration_enabled() {
            return;
        }
        let weak = Arc::downgrade(self);
        let trigger = self.nv_migration_trigger.clone();
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            loop {
                let observed;
                {
                    let Some(fs) = weak.upgrade() else {
                        break;
                    };
                    if fs.is_shutdown() || fs.nv_migration_failed.load(Ordering::Acquire) {
                        break;
                    }
                    observed = fs.nv_migration_epoch.load(Ordering::Acquire);
                    if rnvpc::needs_flush_watermark()
                        || fs.nv_capacity_waiters.load(Ordering::Acquire) != 0
                    {
                        match fs.migrate_nv_round(MIGRATION_BATCH_PAGES) {
                            // Keep the pipeline full while work and credits exist.
                            Ok(submitted) if submitted != 0 => continue,
                            Ok(_) => {}
                            Err(error) => {
                                error!("NV migration failed: {error:?}");
                                fs.fail_nv_migration();
                                break;
                            }
                        }
                    }
                }
                // The timeout only retries idle/stalled candidates. Successful
                // rounds have no timer delay; completion wakes the credit wait.
                let _ = trigger.wait_until_or_timeout(
                    || {
                        weak.upgrade()
                            .is_none_or(|fs| {
                                fs.is_shutdown()
                                    || fs.nv_migration_epoch.load(Ordering::Acquire) != observed
                            })
                            .then_some(())
                    },
                    &Duration::from_millis(10),
                );
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
                        error!("NV completion failed: {error:?}");
                        fs.fail_nv_migration();
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

    pub(super) fn wake_nv_capacity_waiters(&self) {
        if let Some(pool) = rnvpc::pool() {
            pool.wake_data_capacity_waiters();
        }
        self.nv_completion_trigger.wake_all();
    }

    fn fail_nv_migration(&self) {
        self.nv_migration_failed.store(true, Ordering::Release);
        self.wake_nv_capacity_waiters();
        self.request_nv_migration();
    }

    pub(super) fn request_nv_migration(&self) {
        self.nv_migration_epoch.fetch_add(1, Ordering::Release);
        self.nv_migration_trigger.wake_all();
    }

    /// Sleeps outside inode/publication locks until a migration round completes.
    /// Retrying re-reads authority and reserves the exact required page count.
    pub(super) fn wait_for_nv_capacity(&self, pages: usize) -> Result<()> {
        if !self.nv_migration_enabled() {
            return_errno_with_message!(Errno::EAGAIN, "NV migration is disabled");
        }
        self.nv_capacity_waiters.fetch_add(1, Ordering::AcqRel);
        self.request_nv_migration();
        let wait_started = pp::now();
        if let Some(pool) = rnvpc::pool() {
            pool.wait_for_data_capacity(pages, || {
                self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire)
            });
        }
        pp::NV_CAPACITY_WAIT.record_since(wait_started, pages);
        self.nv_capacity_waiters.fetch_sub(1, Ordering::AcqRel);
        if self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "NV migration cannot reclaim capacity");
        }
        Ok(())
    }

    pub(super) fn migrate_nv_round(&self, budget: usize) -> Result<usize> {
        let profile_started = pp::now();
        let _serial = self.nv_migration_lock.lock();
        if self.nv_migration_failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "ambiguous migration target requires recovery");
        }
        let budget = budget.min(MIGRATION_BATCH_PAGES);
        if budget == 0 {
            return Ok(0);
        }
        let Some(pool) = rnvpc::pool() else {
            return Ok(0);
        };
        let candidate_started = pp::now();
        let mut pages = Vec::new();
        if self.rnvpc_extend_enabled() {
            let candidates = rnvpc::rnvpce_flush_candidates(self.rnvpc_fs_id(), budget);
            for candidate in candidates {
                let claimed =
                    match pool.claim_migration_extent(candidate.reference, candidate.reverse) {
                        Ok(claimed) => claimed,
                        Err(_) => {
                            let _ = pool.enqueue_dirty_nvpage(
                                candidate.page,
                                candidate.reference.allocation_seq(),
                            );
                            continue;
                        }
                    };
                if pages.len().saturating_add(claimed.len()) > budget {
                    drop(claimed);
                    let _ = pool
                        .enqueue_dirty_nvpage(candidate.page, candidate.reference.allocation_seq());
                    continue;
                }
                let (identity, claimed_pages) = claimed.into_parts();
                for claimed in claimed_pages {
                    let page = rnvpc::id::NvPageId::try_from(
                        usize::try_from(
                            claimed
                                .reference
                                .slot()
                                .ok_or_else(|| Error::new(Errno::EUCLEAN))?,
                        )
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                    )?;
                    pages.push(PendingPage {
                        inode: claimed.key.inode,
                        index: claimed.key.page_index,
                        page,
                        generation: claimed.reference.allocation_seq(),
                        target: claimed.ssd_block,
                        reference: Some(claimed.reference),
                        extent: Some(identity.clone()),
                        lease: claimed.eviction,
                    });
                }
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
                        extent: None,
                        lease,
                    });
                }
            }
        }
        pp::NV_CANDIDATE_SELECT.record_since(candidate_started, pages.len());
        let target_started = pp::now();
        let metadata_groups = if self.rnvpc_extend_enabled() {
            self.prepare_nv_targets(&mut pages)?
        } else {
            BTreeSet::new()
        };
        pp::NV_TARGET_PREPARE.record_since(target_started, pages.len());
        let submitted = pages.len();
        if submitted == 0 {
            return Ok(0);
        }
        let data_submit_started = pp::now();
        pages.sort_by_key(|page| page.target);
        let max_segments = self
            .block_device()
            .metadata()
            .max_nr_segments_per_bio
            .max(1);
        let mut pending = pages.into_iter().peekable();
        let mut submissions = Vec::new();
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
            submissions.push((target, segments, run));
        }
        let batch = MigrationBatch::new(submissions.len(), submitted);
        self.nv_in_flight.fetch_add(submitted, Ordering::AcqRel);
        let mut submit_error = None;
        for (target, segments, run) in submissions {
            let completed = self.nv_completion_queue.clone();
            let trigger = self.nv_completion_trigger.clone();
            let callback_batch = batch.clone();
            let callback_pages: Arc<SpinLock<Option<Vec<PendingPage>>, LocalIrqDisabled>> =
                Arc::new(SpinLock::new(Some(run)));
            let submitted_pages = callback_pages.clone();
            let callback: BioCompleteFn = Box::new(move |status| {
                // All callback-owned memory was allocated before submission.
                let pages = submitted_pages
                    .lock()
                    .take()
                    .expect("migration BIO callback must run exactly once");
                callback_batch.complete_data(CompletionJob { status, pages }, &completed, &trigger);
            });
            let mut io_batch = IoBatch::new();
            if let Err(error) =
                self.write_segments_async(target, segments, Some(callback), &mut io_batch)
            {
                // Enqueue failure drops the callback without running it. Recover
                // its pages from the shared slot and complete the batch once.
                let pages = callback_pages
                    .lock()
                    .take()
                    .expect("failed migration submission must retain its pages");
                batch.complete_data(
                    CompletionJob {
                        status: BioStatus::IoError,
                        pages,
                    },
                    &self.nv_completion_queue,
                    &self.nv_completion_trigger,
                );
                submit_error = Some(error);
            }
        }
        pp::NV_DATA_SUBMIT.record_since(data_submit_started, submitted);
        let metadata_started = pp::now();
        let metadata_result = if self.rnvpc_extend_enabled() {
            // Data BIOs are already in flight. These nojournal metadata writes
            // may complete in either order and become durable together at the
            // completion worker's single device flush.
            self.sync_rnvpce_allocation_metadata(&metadata_groups)
        } else {
            Ok(())
        };
        pp::NV_METADATA_SUBMIT.record_since(metadata_started, submitted);
        batch.complete_metadata(
            metadata_result.is_ok() && submit_error.is_none(),
            &self.nv_completion_queue,
            &self.nv_completion_trigger,
        );
        if let Some(error) = submit_error {
            return Err(error.into());
        }
        metadata_result?;
        pp::NV_FLUSH_TOTAL.record_since(profile_started, submitted);
        Ok(submitted)
    }

    /// Allocates SSD targets and publishes their identities in NVM.
    /// EXT4 allocation metadata joins the later orderless batch barrier.
    fn prepare_nv_targets(&self, pages: &mut [PendingPage]) -> Result<BTreeSet<usize>> {
        let mut allocated = Vec::with_capacity(pages.len());
        let result = (|| {
            // Adjacent logical pages share a range allocation; preserve existing
            // overwrite targets and consume partial returns under fragmentation.
            pages.sort_by_key(|page| (page.inode, page.index));
            let mut cursor = 0;
            while cursor < pages.len() {
                if pages[cursor].target != 0 {
                    cursor += 1;
                    continue;
                }
                let start = cursor;
                let mut end = start + 1;
                while end < pages.len()
                    && pages[end].target == 0
                    && pages[end].inode == pages[start].inode
                    && pages[end].index == pages[end - 1].index + 1
                {
                    end += 1;
                }
                let op = self.begin_op(self.single_block_map_credits(0))?;
                let started = batch_metrics::now();
                let blocks = self.alloc_blocks_with_intent(
                    (end - start) as u32,
                    0,
                    op.get(),
                    AllocIntent::RnvpceReserved,
                )?;
                batch_metrics::elapsed(Phase::Allocate, started);
                batch_metrics::count(Event::AllocatedPages, (blocks.end - blocks.start) as usize);
                for target in blocks {
                    allocated.push((cursor, target));
                    cursor += 1;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // No NV descriptor names these targets yet. Roll back every
            // private allocation, including a partially prepared round.
            let rollback = (|| {
                for (_, target) in &allocated {
                    let op = self.begin_op(self.single_block_map_credits(0))?;
                    self.free_blocks(
                        super::journal::BlockFreeAuth::without_revoke_duty(*target, 1),
                        op.get(),
                    )?;
                }
                Ok::<(), Error>(())
            })();
            if let Err(rollback_error) = rollback {
                self.fail_nv_migration();
                error!("NV target rollback durability failed: {rollback_error:?}");
            }
            return Err(error);
        }
        let targets: Result<Vec<_>> = allocated
            .iter()
            .map(|(index, target)| {
                Ok((
                    pages[*index]
                        .reference
                        .ok_or_else(|| Error::new(Errno::EUCLEAN))?,
                    *target,
                ))
            })
            .collect();
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let started = batch_metrics::now();
        let targets = targets?;
        for targets in targets.chunks(rnvpc::write_transaction::SSD_MIGRATION_BATCH_PAGES) {
            if let Err(error) = pool.assign_migration_targets(targets, None) {
                // Some selectors may already be durable: never free possibly named blocks.
                self.fail_nv_migration();
                return Err(error);
            }
        }
        batch_metrics::elapsed(Phase::TargetPublish, started);
        batch_metrics::count(Event::TargetPages, allocated.len());
        let groups = allocated
            .iter()
            .map(|(_, target)| self.block_group_index_for_block(*target))
            .collect::<Result<BTreeSet<_>>>()?;
        for (index, target) in allocated {
            pages[index].target = target;
        }
        Ok(groups)
    }

    pub(super) fn process_nv_completions(&self) -> Result<usize> {
        let _serial = self.nv_completion_lock.lock();
        // Move the whole backing queue to process context without allocating
        // or releasing object leases while holding its IRQ-safe spinlock.
        let mut drained = VecDeque::new();
        core::mem::swap(&mut *self.nv_completion_queue.lock(), &mut drained);
        if drained.is_empty() {
            return Ok(0);
        }
        let count = drained
            .iter()
            .map(|batch| batch.page_count())
            .sum::<usize>();
        let mut failure = None;
        while let Some(batch) = drained.pop_front() {
            if let Err(error) = self.finish_nv_completion_batch(batch) {
                failure = Some(error);
            }
        }
        drop(drained);
        self.nv_in_flight.fetch_sub(count, Ordering::AcqRel);
        self.request_nv_migration();
        self.wake_nv_capacity_waiters();
        match failure {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    fn finish_nv_completion_batch(&self, batch: Arc<MigrationBatch>) -> Result<()> {
        let completion_started = batch.completion_started();
        let page_count = batch.page_count();
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let (jobs, metadata_failed) = batch.take_jobs();
        if metadata_failed || jobs.iter().any(|job| job.status != BioStatus::Complete) {
            return_errno_with_message!(Errno::EIO, "orderless migration batch I/O failed");
        }
        let started = batch_metrics::now();
        let device_flush_started = pp::now();
        if self.rnvpc_extend_enabled() {
            if self.block_device().sync()? != BioStatus::Complete {
                return_errno!(Errno::EIO);
            }
        } else {
            self.persist_nv_allocations()?;
        }
        pp::NV_DEVICE_FLUSH.record_since(device_flush_started, page_count);
        batch_metrics::elapsed(Phase::DataBarrier, started);
        let mut groups: BTreeMap<u64, Vec<PendingPage>> = BTreeMap::new();
        let mut failure = None;
        for job in jobs {
            for page in job.pages {
                groups.entry(page.inode).or_default().push(page);
            }
        }
        for (inode_id, pages) in groups {
            if self.rnvpc_extend_enabled() {
                let publish_started = pp::now();
                let mut extents: BTreeMap<u64, (Arc<MigrationExtentIdentity>, Vec<PendingPage>)> =
                    BTreeMap::new();
                for page in pages {
                    let identity = page
                        .extent
                        .clone()
                        .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                    if identity.inode() != inode_id || identity.fs_id() != self.rnvpc_fs_id() {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "migration extent owner changed"
                        );
                    }
                    let entry = extents
                        .entry(identity.logical_start())
                        .or_insert_with(|| (identity.clone(), Vec::new()));
                    if !Arc::ptr_eq(&entry.0, &identity) {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "duplicate migration extent identity"
                        );
                    }
                    entry.1.push(page);
                }
                let published_pages = extents.values().map(|(_, pages)| pages.len()).sum();
                for (identity, mut pages) in extents.into_values() {
                    pages.sort_by_key(|page| page.index);
                    let updates: Vec<_> = pages
                        .iter()
                        .map(|page| {
                            Ok(rnvpc::write_transaction::SsdMigrationPage {
                                logical_block: page.index,
                                reference: page
                                    .reference
                                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?,
                                ssd_block: page.target,
                            })
                        })
                        .collect::<Result<_>>()?;
                    let results = match rnvpc::write_transaction::migrate_claimed_nv_extent_to_ssd(
                        pool, &identity, &updates, None,
                    ) {
                        Ok(results) => results,
                        Err(error) if error.error() == Errno::ESTALE => {
                            let still_authoritative = updates.iter().all(|update| {
                                rnvpc::write_transaction::mapped_nv_page_ref(
                                    pool,
                                    self.rnvpc_fs_id(),
                                    inode_id,
                                    update.logical_block,
                                )
                                .ok()
                                .flatten()
                                    == Some(update.reference)
                            });
                            if !still_authoritative {
                                return_errno_with_message!(
                                    Errno::EUCLEAN,
                                    "stale migration extent is no longer authoritative"
                                );
                            }
                            let retry: Vec<_> = pages
                                .iter()
                                .map(|page| (page.page, page.generation))
                                .collect();
                            drop(pages);
                            for (page, generation) in retry {
                                pool.enqueue_dirty_nvpage(page, generation)?;
                            }
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let mut reclaim = Vec::with_capacity(pages.len());
                    let mut released = 0;
                    for (page, result) in pages.into_iter().zip(results) {
                        match result {
                            Ok(consumed) => {
                                released += usize::from(consumed);
                                reclaim.push((page.lease, page.reference.unwrap()));
                            }
                            Err(error) => failure = Some(error),
                        }
                    }
                    if released != 0 {
                        self.release_rnvpce_blocks(released as u64)?;
                    }
                    let started = batch_metrics::now();
                    let reclaim_started = pp::now();
                    let reclaimed = reclaim.len();
                    if let Err(error) = pool.reclaim_migrated_pages(reclaim, None) {
                        failure = Some(error);
                    } else {
                        batch_metrics::count(Event::ReclaimedPages, reclaimed);
                    }
                    pp::NV_RECLAIM.record_since(reclaim_started, reclaimed);
                    batch_metrics::elapsed(Phase::Reclaim, started);
                }
                pp::NV_EXTENT_PUBLISH.record_since(publish_started, published_pages);
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
        pp::NV_COMPLETION_TOTAL.record_since(completion_started, page_count);
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
            let submitted = self.migrate_nv_round(MIGRATION_BATCH_PAGES)?;
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
        self.persist_nv_allocations()?;
        batch_metrics::dump();
        Ok(())
    }
}
