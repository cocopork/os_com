// SPDX-License-Identifier: MPL-2.0

//! Two-worker, bounded asynchronous NV migration. BIO callbacks enqueue only;
//! process-context completion owns durability and publishes reclaim tickets;
//! foreground or background helpers own SSD publication and NVPage reclamation.
use core::sync::atomic::{AtomicUsize, Ordering};

use aster_block::bio::BioDirection;
use ostd::sync::LocalIrqDisabled;

use super::{fs::Ext4, pipeline_profile as pp, prelude::*};
use crate::fs::rnvpc::{
    self,
    baseline_generation::BaselineRangeToken,
    batch_metrics::{self, Event, Phase},
    frame::{ClaimedDirtyExtent, EvictingDataPage},
    metadata::PersistentObjectRef,
    redirect_index::DescriptorHandle,
};

mod batch;
mod reclaim;

use batch::MigrationBatch;
use reclaim::ReclaimReadyQueue;

const MIGRATION_BATCH_PAGES: usize = super::writeback::WRITEBACK_BATCH;
pub(super) type CompletionQueue = Arc<SpinLock<VecDeque<Arc<MigrationBatch>>, LocalIrqDisabled>>;

#[derive(Default)]
pub(super) struct MigrationGroupOwners {
    active: BTreeSet<usize>,
}

impl MigrationGroupOwners {
    fn try_acquire(&mut self, groups: &BTreeSet<usize>) -> bool {
        if groups.iter().any(|group| self.active.contains(group)) {
            return false;
        }
        self.active.extend(groups.iter().copied());
        true
    }

    fn release(&mut self, groups: &BTreeSet<usize>) {
        for group in groups {
            self.active.remove(group);
        }
    }

    pub(super) fn contains(&self, group: usize) -> bool {
        self.active.contains(&group)
    }

    fn try_acquire_for_allocation(
        &mut self,
        required: &BTreeSet<usize>,
        candidates: &[(usize, u32)],
        needed_blocks: usize,
    ) -> Option<BTreeSet<usize>> {
        if required.iter().any(|group| self.active.contains(group)) {
            return None;
        }
        let mut selected = required.clone();
        let mut available = candidates
            .iter()
            .filter(|(group, _)| selected.contains(group))
            .map(|(_, free)| *free as usize)
            .sum::<usize>();
        for &(group, free) in candidates {
            if available >= needed_blocks {
                break;
            }
            if selected.contains(&group) || self.active.contains(&group) || free == 0 {
                continue;
            }
            selected.insert(group);
            available = available.saturating_add(free as usize);
        }
        if available < needed_blocks {
            return None;
        }
        self.active.extend(selected.iter().copied());
        Some(selected)
    }
}

struct MigrationGroupReservation<'a> {
    owners: &'a Mutex<MigrationGroupOwners>,
    groups: Option<BTreeSet<usize>>,
}

impl<'a> MigrationGroupReservation<'a> {
    fn new(owners: &'a Mutex<MigrationGroupOwners>, groups: BTreeSet<usize>) -> Self {
        Self {
            owners,
            groups: Some(groups),
        }
    }

    fn transfer_to_batch(mut self) -> BTreeSet<usize> {
        self.groups.take().unwrap()
    }

    fn groups(&self) -> &BTreeSet<usize> {
        self.groups
            .as_ref()
            .expect("live migration group reservation has groups")
    }
}

impl Drop for MigrationGroupReservation<'_> {
    fn drop(&mut self) {
        if let Some(groups) = self.groups.take() {
            self.owners.lock().release(&groups);
        }
    }
}
pub(super) struct CompletionJob {
    status: BioStatus,
    pages: Vec<PendingPage>,
}
pub(super) type ReclaimQueue = Arc<ReclaimReadyQueue<ReclaimTicket>>;

pub(super) fn reclaim_queue() -> ReclaimQueue {
    Arc::new(ReclaimReadyQueue::new())
}

#[cfg(ktest)]
pub(super) fn assert_reclaim_queue_contract() {
    reclaim::assert_queue_contract();
}

pub(super) struct ReclaimTicket {
    pages: Vec<PendingPage>,
    batch: Arc<ReclaimBatch>,
    ready_at_ns: u64,
}

struct ReclaimBatch {
    remaining_tickets: AtomicUsize,
    migration_groups: BTreeSet<usize>,
}

impl ReclaimBatch {
    fn new(tickets: usize, migration_groups: BTreeSet<usize>) -> Arc<Self> {
        debug_assert!(tickets != 0);
        Arc::new(Self {
            remaining_tickets: AtomicUsize::new(tickets),
            migration_groups,
        })
    }

    fn complete_ticket(&self, fs: &Ext4) {
        if self.remaining_tickets.fetch_sub(1, Ordering::AcqRel) == 1 {
            fs.nv_migration_groups
                .lock()
                .release(&self.migration_groups);
            fs.request_nv_migration();
        }
    }
}

struct PendingPage {
    inode: u64,
    index: u64,
    page: rnvpc::id::NvPageId,
    generation: u64,
    target: u64,
    reference: Option<PersistentObjectRef>,
    needs_baseline: bool,
    redirect: Option<(DescriptorHandle, Arc<BaselineRangeToken>)>,
    lease: MigrationLease,
}

enum MigrationLease {
    Legacy(EvictingDataPage),
    Redirect {
        extent: Arc<ClaimedDirtyExtent>,
        offset: usize,
    },
}

impl MigrationLease {
    fn dma_segment(&self) -> Result<ostd::mm::USegment> {
        match self {
            Self::Legacy(lease) => lease.dma_segment(),
            Self::Redirect { extent, offset } => extent.dma_segment(*offset),
        }
    }

    fn dirty_extent(&self) -> Option<&Arc<ClaimedDirtyExtent>> {
        match self {
            Self::Legacy(_) => None,
            Self::Redirect { extent, .. } => Some(extent),
        }
    }
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
                        || fs.nv_migration_progress_waiters.load(Ordering::Acquire) != 0
                    {
                        match fs.migrate_nv_round(MIGRATION_BATCH_PAGES) {
                            // Keep the pipeline full while work and credits exist.
                            Ok(submitted) if submitted != 0 => continue,
                            Ok(_) => {}
                            Err(error) if error.error() == Errno::EAGAIN => {}
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
        let trigger = self.nv_reclaim_trigger.clone();
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            loop {
                let Some(fs) = weak.upgrade() else {
                    break;
                };
                if fs.nv_reclaim_queue.ready_units() != 0
                    && fs.nv_capacity_waiters.load(Ordering::Acquire) <= 1
                {
                    if let Err(error) = fs.help_nv_reclaim(MIGRATION_BATCH_PAGES) {
                        error!("NV parallel reclaim failed: {error:?}");
                    }
                    continue;
                }
                if (fs.is_shutdown() || fs.nv_migration_failed.load(Ordering::Acquire))
                    && fs.nv_reclaim_pending.load(Ordering::Acquire) == 0
                {
                    break;
                }
                drop(fs);
                let _ = trigger.wait_until(|| {
                    weak.upgrade()
                        .is_none_or(|fs| {
                            (fs.nv_reclaim_queue.ready_units() != 0
                                && fs.nv_capacity_waiters.load(Ordering::Acquire) <= 1)
                                || ((fs.is_shutdown()
                                    || fs.nv_migration_failed.load(Ordering::Acquire))
                                    && fs.nv_reclaim_pending.load(Ordering::Acquire) == 0)
                        })
                        .then_some(())
                });
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
        let weak = Arc::downgrade(self);
        let trigger = self.nv_allocation_pager_trigger.clone();
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            loop {
                let Some(fs) = weak.upgrade() else {
                    break;
                };
                if fs.is_shutdown() || fs.nv_migration_failed.load(Ordering::Acquire) {
                    break;
                }
                let observed = fs.nv_allocation_pager_epoch.load(Ordering::Acquire);
                match fs.writeback_one_allocation_group() {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        error!("allocation metadata pager failed: {error:?}");
                        fs.fail_nv_migration();
                        break;
                    }
                }
                drop(fs);
                let _ = trigger.wait_until_or_timeout(
                    || {
                        weak.upgrade()
                            .is_none_or(|fs| {
                                fs.is_shutdown()
                                    || fs.nv_allocation_pager_epoch.load(Ordering::Acquire)
                                        != observed
                            })
                            .then_some(())
                    },
                    &Duration::from_millis(100),
                );
            }
        })
        .spawn();
    }

    pub(super) fn request_allocation_writeback(&self) {
        self.nv_allocation_pager_epoch
            .fetch_add(1, Ordering::Release);
        self.nv_allocation_pager_trigger.wake_one();
    }

    fn writeback_one_allocation_group(&self) -> Result<bool> {
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let cache = rnvpc::allocation_metadata::AllocationMetadataCache::new(pool.clone());
        let mut resident = cache.resident_groups(self.rnvpc_fs_id())?;
        if resident.len() < pool.allocation_group_cache_limit() {
            return Ok(false);
        }
        // Full groups cannot satisfy a future allocation. Otherwise evict the
        // group with the least remaining space before broad free groups.
        resident.sort_by_key(|group| (group.free_blocks != 0, group.free_blocks, group.group));
        let victim = resident[0];
        let _migration = self.nv_migration_lock.lock();
        let Some(current) = cache.selected_image(self.rnvpc_fs_id(), victim.group)? else {
            return Ok(true);
        };
        match self.writeback_rnvpce_allocation_group(victim.group, current.generation()) {
            Ok(()) => {}
            Err(error) if matches!(error.error(), Errno::ESTALE | Errno::EBUSY) => {}
            Err(error) => return Err(error),
        }
        Ok(true)
    }

    pub(super) fn wake_nv_capacity_waiters(&self) {
        if let Some(pool) = rnvpc::pool() {
            pool.wake_data_capacity_waiters();
        }
        self.nv_completion_trigger.wake_all();
        self.nv_reclaim_trigger.wake_all();
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
        let wait_started = pp::now();
        let result: Result<()> = (|| {
            let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
            loop {
                if pool.can_reserve_data_pages(pages) {
                    break;
                }
                if self.nv_capacity_waiters.load(Ordering::Acquire) > 1
                    && self.help_nv_reclaim(pages.max(1))? != 0
                {
                    continue;
                }
                self.request_nv_migration();
                pool.wait_for_data_capacity_or(
                    pages,
                    || self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire),
                    || self.nv_reclaim_queue.ready_units() != 0,
                );
                if self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire) {
                    break;
                }
            }
            Ok(())
        })();
        pp::NV_CAPACITY_WAIT.record_since(wait_started, pages);
        self.nv_capacity_waiters.fetch_sub(1, Ordering::AcqRel);
        if self.nv_reclaim_queue.ready_units() != 0 {
            self.nv_reclaim_trigger.wake_one();
        }
        result?;
        if self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "NV migration cannot reclaim capacity");
        }
        Ok(())
    }

    /// Forces progress for a page whose redirect is already owned by migration.
    /// This is distinct from capacity pressure: below the high watermark, only
    /// an explicit waiter makes the worker drain such a page.
    pub(super) fn wait_for_nv_migration_progress(
        &self,
        pool: &rnvpc::frame::Rnvpc,
        lease_epoch: u64,
    ) -> Result<()> {
        if !self.nv_migration_enabled() {
            return_errno_with_message!(Errno::EAGAIN, "NV migration is disabled");
        }
        self.nv_migration_progress_waiters
            .fetch_add(1, Ordering::AcqRel);
        self.request_nv_migration();
        pool.wait_for_data_page_lease_change_until(lease_epoch, || {
            self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire)
        });
        self.nv_migration_progress_waiters
            .fetch_sub(1, Ordering::AcqRel);
        if self.is_shutdown() || self.nv_migration_failed.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "NV migration cannot make progress");
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
            for lease in pool.claim_ready_dirty_extents(self.rnvpc_fs_id(), budget)? {
                let claimed = match pool.claim_dirty_extent(lease) {
                    Ok(claimed) => Arc::new(claimed),
                    Err(error)
                        if error.error() == Errno::EBUSY || error.error() == Errno::ESTALE =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                batch_metrics::count(Event::CandidateRanges, 1);
                batch_metrics::count(Event::CandidatePages, claimed.pages().len());
                for (offset, handle) in claimed.pages().iter().copied().enumerate() {
                    pages.push(PendingPage {
                        inode: claimed.inode(),
                        index: claimed.logical_start() + offset as u64,
                        page: claimed.page_id(offset)?,
                        generation: handle.reference.allocation_seq(),
                        target: handle.ssd_target,
                        reference: Some(handle.reference),
                        needs_baseline: handle.ssd_target == 0 || handle.migration_target,
                        redirect: Some((handle, claimed.baseline_token.clone())),
                        lease: MigrationLease::Redirect {
                            extent: claimed.clone(),
                            offset,
                        },
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
                        needs_baseline: false,
                        redirect: None,
                        lease: MigrationLease::Legacy(lease),
                    });
                }
            }
        }
        pp::NV_CANDIDATE_SELECT.record_since(candidate_started, pages.len());
        let target_started = pp::now();
        let group_reservation = if self.rnvpc_extend_enabled() {
            match self.prepare_nv_targets(&mut pages) {
                Ok(groups) if groups.is_empty() => None,
                Ok(groups) => Some(MigrationGroupReservation::new(
                    &self.nv_migration_groups,
                    groups,
                )),
                Err(error) if error.error() == Errno::EBUSY => {
                    let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
                    self.restore_failed_migration_jobs(
                        pool,
                        vec![CompletionJob {
                            status: BioStatus::IoError,
                            pages,
                        }],
                    );
                    return_errno_with_message!(
                        Errno::EAGAIN,
                        "allocation group migration is deferred"
                    );
                }
                Err(error) => {
                    let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
                    self.restore_failed_migration_jobs(
                        pool,
                        vec![CompletionJob {
                            status: BioStatus::IoError,
                            pages,
                        }],
                    );
                    return Err(error);
                }
            }
        } else {
            None
        };
        pp::NV_TARGET_PREPARE.record_since(target_started, pages.len());
        let submitted = pages.len();
        if submitted == 0 {
            debug_assert!(group_reservation.is_none());
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
        let batch = MigrationBatch::new(
            submissions.len(),
            submitted,
            group_reservation
                .map_or_else(BTreeSet::new, |reservation| reservation.transfer_to_batch()),
        );
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
        if let Some(error) = submit_error {
            return Err(error.into());
        }
        pp::NV_FLUSH_TOTAL.record_since(profile_started, submitted);
        Ok(submitted)
    }

    fn reserve_nv_target_groups(
        &self,
        pages: &[PendingPage],
    ) -> Result<MigrationGroupReservation<'_>> {
        let required = pages
            .iter()
            .filter_map(|page| (page.target != 0).then_some(page.target))
            .map(|target| self.block_group_index_for_block(target))
            .collect::<Result<BTreeSet<_>>>()?;
        let needed = pages.iter().filter(|page| page.target == 0).count();
        let nr_groups = self.super_block().nr_block_groups() as usize;
        let mut candidates = (0..nr_groups)
            .map(|group| (group, self.block_group(group).free_blocks_count()))
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_group, left_free), (right_group, right_free)| {
            let left_fits = (*left_free as usize) >= needed;
            let right_fits = (*right_free as usize) >= needed;
            right_fits
                .cmp(&left_fits)
                .then_with(|| right_free.cmp(left_free))
                .then_with(|| left_group.cmp(right_group))
        });
        let total_free = candidates
            .iter()
            .map(|(_, free)| *free as usize)
            .sum::<usize>();
        let mut owners = self.nv_migration_groups.lock();
        if required.iter().any(|group| owners.contains(*group)) {
            return_errno_with_message!(Errno::EBUSY, "allocation group is already migrating");
        }
        if total_free < needed {
            return_errno!(Errno::ENOSPC);
        }
        let groups = owners
            .try_acquire_for_allocation(&required, &candidates, needed)
            .ok_or_else(|| Error::new(Errno::EBUSY))?;
        drop(owners);
        Ok(MigrationGroupReservation::new(
            &self.nv_migration_groups,
            groups,
        ))
    }

    /// Allocates SSD targets and publishes their identities in NVM.
    /// Group ownership precedes every bitmap/GDT mutation, and EXT4 allocation
    /// metadata joins the later orderless batch barrier.
    fn prepare_nv_targets(&self, pages: &mut [PendingPage]) -> Result<BTreeSet<usize>> {
        let group_reservation = self.reserve_nv_target_groups(pages)?;
        let groups = group_reservation.groups().clone();
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
                let blocks =
                    self.alloc_rnvpce_blocks_in_groups((end - start) as u32, &groups, op.get())?;
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
        // NVM allocation authority is durable before any descriptor can name
        // a newly allocated SSD target. No bitmap/GDT/superblock SSD I/O occurs.
        if let Err(error) = self.persist_rnvpce_allocation_metadata(&groups) {
            for (_, target) in &allocated {
                let op = self.begin_op(self.single_block_map_credits(0))?;
                self.free_blocks(
                    super::journal::BlockFreeAuth::without_revoke_duty(*target, 1),
                    op.get(),
                )?;
            }
            // A partially published generation may have committed before the
            // error. Publish the rolled-back bitmap as the next authority.
            if let Err(rollback_error) = self.persist_rnvpce_allocation_metadata(&groups) {
                self.fail_nv_migration();
                error!("NV allocation rollback publication failed: {rollback_error:?}");
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
        let Some(pool) = rnvpc::pool() else {
            return_errno!(Errno::EINVAL);
        };
        let started = batch_metrics::now();
        let targets = match targets {
            Ok(targets) => targets,
            Err(error) => return Err(error),
        };
        for targets in targets.chunks(rnvpc::write_transaction::SSD_MIGRATION_BATCH_PAGES) {
            if let Err(error) = pool.assign_migration_targets(targets, None) {
                // Some selectors may already be durable: never free possibly named blocks.
                self.fail_nv_migration();
                return Err(error);
            }
        }
        batch_metrics::elapsed(Phase::TargetPublish, started);
        batch_metrics::count(Event::TargetPages, allocated.len());
        for (index, target) in allocated {
            pages[index].target = target;
            if let Some((handle, _)) = pages[index].redirect.as_mut() {
                handle.ssd_target = target;
                handle.migration_target = true;
            }
        }
        Ok(group_reservation.transfer_to_batch())
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
            let groups = batch.migration_groups().clone();
            match self.finish_nv_completion_batch(batch) {
                Ok(true) => {}
                Ok(false) => self.nv_migration_groups.lock().release(&groups),
                Err(error) => {
                    self.nv_migration_groups.lock().release(&groups);
                    failure = Some(error);
                }
            }
        }
        drop(drained);
        self.nv_in_flight.fetch_sub(count, Ordering::AcqRel);
        self.request_nv_migration();
        match failure {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    fn finish_nv_completion_batch(&self, batch: Arc<MigrationBatch>) -> Result<bool> {
        let completion_started = batch.completion_started();
        let page_count = batch.page_count();
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let jobs = batch.take_jobs();
        if jobs.iter().any(|job| job.status != BioStatus::Complete) {
            self.restore_failed_migration_jobs(pool, jobs);
            return_errno_with_message!(Errno::EIO, "orderless migration batch I/O failed");
        }
        let started = batch_metrics::now();
        let device_flush_started = pp::now();
        if self.rnvpc_extend_enabled() {
            match self.block_device().sync() {
                Ok(BioStatus::Complete) => {}
                Ok(_) => {
                    self.restore_failed_migration_jobs(pool, jobs);
                    return_errno!(Errno::EIO);
                }
                Err(error) => {
                    self.restore_failed_migration_jobs(pool, jobs);
                    return Err(error.into());
                }
            }
        } else {
            self.persist_nv_allocations()?;
        }
        pp::NV_DEVICE_FLUSH.record_since(device_flush_started, page_count);
        batch_metrics::elapsed(Phase::DataBarrier, started);
        let mut groups: BTreeMap<(u64, u64, u64), Vec<PendingPage>> = BTreeMap::new();
        let mut failure = None;
        for job in jobs {
            for page in job.pages {
                let identity = page
                    .lease
                    .dirty_extent()
                    .map_or((page.inode, page.index, 0), |extent| {
                        (extent.inode(), extent.logical_start(), extent.generation())
                    });
                groups.entry(identity).or_default().push(page);
            }
        }
        if self.rnvpc_extend_enabled() {
            if groups.is_empty() {
                pp::NV_COMPLETION_TOTAL.record_since(completion_started, page_count);
                return Ok(false);
            }
            if groups
                .values()
                .any(|pages| pages.iter().any(|page| page.redirect.is_none()))
            {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "rNVPCe migration reached a legacy extent candidate"
                );
            }
            let reclaim_batch = ReclaimBatch::new(groups.len(), batch.migration_groups().clone());
            for (_identity, pages) in groups {
                let pages_count = pages.len();
                self.nv_reclaim_pending
                    .fetch_add(pages_count, Ordering::AcqRel);
                self.nv_reclaim_queue.publish(
                    ReclaimTicket {
                        pages,
                        batch: reclaim_batch.clone(),
                        ready_at_ns: pp::now(),
                    },
                    pages_count,
                );
                if self.nv_capacity_waiters.load(Ordering::Acquire) <= 1 {
                    self.nv_reclaim_trigger.wake_one();
                }
                pool.wake_one_data_capacity_waiter();
            }
            pp::NV_COMPLETION_TOTAL.record_since(completion_started, page_count);
            return Ok(true);
        }
        for (_identity, pages) in groups {
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
                        let MigrationLease::Legacy(lease) = page.lease else {
                            return_errno_with_message!(
                                Errno::EUCLEAN,
                                "redirect lease reached rNVPC completion"
                            );
                        };
                        lease.finish(None)?;
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
        if let Some(error) = failure {
            return Err(error);
        }
        pp::NV_COMPLETION_TOTAL.record_since(completion_started, page_count);
        Ok(false)
    }

    pub(super) fn help_nv_reclaim(&self, budget: usize) -> Result<usize> {
        let mut reclaimed = 0usize;
        while reclaimed < budget {
            let Some((ticket, pages_count)) = self.nv_reclaim_queue.claim() else {
                break;
            };
            let ReclaimTicket {
                pages,
                batch,
                ready_at_ns,
            } = ticket;
            pp::NV_RECLAIM_READY_WAIT.record_since(ready_at_ns, pages_count);
            let publish_started = pp::now();
            let result = self.finish_redirect_pages(
                rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?,
                pages,
            );
            if let Err(error) = result {
                self.nv_reclaim_pending
                    .fetch_sub(pages_count, Ordering::AcqRel);
                batch.complete_ticket(self);
                self.fail_nv_migration();
                return Err(error);
            }
            pp::NV_EXTENT_PUBLISH.record_since(publish_started, pages_count);
            pp::NV_RECLAIM_HELP.record_since(publish_started, pages_count);
            self.nv_reclaim_pending
                .fetch_sub(pages_count, Ordering::AcqRel);
            batch.complete_ticket(self);
            reclaimed = reclaimed.saturating_add(pages_count);
            self.nv_reclaim_trigger.wake_all();
        }
        Ok(reclaimed)
    }

    fn restore_failed_migration_jobs(
        &self,
        _pool: &Arc<rnvpc::frame::Rnvpc>,
        jobs: Vec<CompletionJob>,
    ) {
        for job in jobs {
            for page in job.pages {
                drop(page);
            }
        }
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
        while self.nv_in_flight.load(Ordering::Acquire) != 0
            || self.nv_reclaim_pending.load(Ordering::Acquire) != 0
        {
            if let Err(error) = self.process_nv_completions() {
                failure = Some(error);
            }
            if let Err(error) = self.help_nv_reclaim(MIGRATION_BATCH_PAGES) {
                failure = Some(error);
            }
            let _ = self.nv_completion_trigger.wait_until_or_timeout(
                || {
                    ((self.nv_in_flight.load(Ordering::Acquire) == 0
                        && self.nv_reclaim_pending.load(Ordering::Acquire) == 0)
                        || !self.nv_completion_queue.lock().is_empty()
                        || self.nv_reclaim_queue.ready_units() != 0)
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
            let (submitted, deferred) = match self.migrate_nv_round(MIGRATION_BATCH_PAGES) {
                Ok(submitted) => (submitted, false),
                Err(error) if error.error() == Errno::EAGAIN => (0, true),
                Err(error) => return Err(error),
            };
            self.drain_nv_in_flight()?;
            if deferred {
                continue;
            }
            if submitted == 0 {
                break;
            }
        }
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        for ino in pool.fixed_inode_ids(self.rnvpc_fs_id())? {
            self.read_inode(ino.try_into().map_err(|_| Error::new(Errno::EOVERFLOW))?)?
                .export_rnvpce_extents(self)?;
        }
        // Serialize the final allocation-metadata writeback against target assignment.
        // After the device flush, exact-generation transitions transfer
        // authority back to SSD and release the resident bitmap chunks.
        let _migration = self.nv_migration_lock.lock();
        let cache = rnvpc::allocation_metadata::AllocationMetadataCache::new(pool.clone());
        // Export may allocate native extent metadata after the last rTIO
        // target publication. Refresh every resident group so the generation
        // written back below names the exact post-export allocation image.
        let resident = cache.resident_groups(self.rnvpc_fs_id())?;
        for group in resident {
            self.writeback_rnvpce_allocation_group(group.group, group.generation)?;
        }
        batch_metrics::dump();
        Ok(())
    }

    fn finish_redirect_pages(
        &self,
        pool: &Arc<rnvpc::frame::Rnvpc>,
        pages: Vec<PendingPage>,
    ) -> Result<()> {
        let extent = pages
            .first()
            .and_then(|page| page.lease.dirty_extent())
            .cloned()
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        if pages.len() != extent.pages().len()
            || pages.iter().any(|page| {
                page.lease
                    .dirty_extent()
                    .is_none_or(|candidate| !Arc::ptr_eq(candidate, &extent))
            })
        {
            return_errno_with_message!(Errno::EUCLEAN, "reclaim ticket mixes migration extents");
        }
        let mut exact = Vec::with_capacity(pages.len());
        let mut new_baselines = Vec::new();
        let mut new_target_refs = Vec::new();
        let mut baseline_valid = true;
        let compare_started = batch_metrics::now();
        for page in &pages {
            let (handle, token) = page
                .redirect
                .as_ref()
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            let handle = *handle;
            let key = rnvpc::index::PageKey::new(self.rnvpc_fs_id(), page.inode, page.index);
            let token_matches = token.target(page.index).is_some_and(|target| {
                target
                    == if handle.migration_target {
                        0
                    } else {
                        handle.ssd_target
                    }
            });
            let generation_matches = pool.validate_baseline_token(token);
            if !generation_matches || !token_matches {
                let resolved = rnvpc::write_transaction::current_page_mapping(
                    pool,
                    self.rnvpc_fs_id(),
                    page.inode,
                    page.index,
                )?;
                baseline_valid &= !page.needs_baseline
                    && resolved.is_some_and(|mapping| {
                        mapping.is_written() && mapping.ssd_block() == page.target
                    });
            }
            exact.push((key, handle));
            if page.needs_baseline {
                if page.target == 0 {
                    return_errno_with_message!(Errno::EUCLEAN, "migration has no SSD target");
                }
                new_baselines.push((page.index, page.target));
                new_target_refs.push(handle.reference);
            }
        }
        batch_metrics::elapsed(Phase::CompletionCompare, compare_started);
        batch_metrics::count(Event::CompletionComparisons, exact.len());
        if !baseline_valid {
            drop(pages);
            return Ok(());
        }
        if !new_baselines.is_empty() {
            new_baselines.sort_unstable_by_key(|(logical, _)| *logical);
            let inode = pages[0].inode;
            rnvpc::write_transaction::publish_claimed_ssd_baseline_batch(
                pool,
                self.rnvpc_fs_id(),
                inode,
                &new_baselines,
            )?;
            let mut releasable = Vec::with_capacity(new_target_refs.len());
            for reference in new_target_refs {
                if !pool.persistent_data_has_migration_target(reference)? {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "published SSD baseline has no migration target"
                    );
                }
                if !pool.persistent_data_migration_credit_released(reference)? {
                    releasable.push(reference);
                }
            }
            for references in releasable.chunks(128) {
                pool.mark_migration_credits_released(references, None)?;
            }
            if !releasable.is_empty() {
                rnvpc::write_transaction::consume_ssd_capacity_locked(
                    pool,
                    self.rnvpc_fs_id(),
                    inode,
                    releasable.len() as u64,
                )?;
                self.release_rnvpce_blocks(releasable.len() as u64)?;
            }
        }
        extent.release_physical_for_completion()?;
        for chunk in exact.chunks(128) {
            let reclaimed = loop {
                let lease_epoch = pool.data_page_lease_epoch();
                match pool.clear_redirects_exact(chunk, None) {
                    Ok(reclaimed) => break reclaimed,
                    Err(error) if error.error() == Errno::EBUSY => {
                        pool.wait_for_data_page_lease_change(lease_epoch);
                    }
                    Err(error) => return Err(error),
                }
            };
            let mut slots = reclaimed
                .iter()
                .map(|page| page.index())
                .collect::<Vec<_>>();
            slots.sort_unstable();
            let ranges = slots
                .iter()
                .enumerate()
                .filter(|(index, slot)| *index == 0 || **slot != slots[*index - 1] + 1)
                .count();
            batch_metrics::count(Event::ReclaimedSlotRanges, ranges);
            pool.free_data_pages(&reclaimed)?;
            batch_metrics::count(Event::ReclaimedPages, reclaimed.len());
        }
        extent.complete_success()?;
        drop(pages);
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn rnvpce_block_group_has_one_migration_owner() {
        let mut owners = MigrationGroupOwners::default();
        let first = BTreeSet::from([2usize, 5]);
        let disjoint = BTreeSet::from([7usize]);
        assert!(owners.try_acquire(&first));
        assert!(!owners.try_acquire(&BTreeSet::from([5usize, 6])));
        assert!(owners.try_acquire(&disjoint));
        owners.release(&first);
        assert!(owners.try_acquire(&BTreeSet::from([5usize, 6])));
    }

    #[ktest]
    fn rnvpce_reserves_target_groups_before_allocation() {
        let mut owners = MigrationGroupOwners::default();
        assert!(owners.try_acquire(&BTreeSet::from([1usize])));

        let selected = owners
            .try_acquire_for_allocation(&BTreeSet::new(), &[(1, 128), (2, 96), (3, 64)], 128)
            .unwrap();
        assert_eq!(selected, BTreeSet::from([2usize, 3]));
        assert!(owners.contains(1));
        assert!(owners.contains(2));
        assert!(owners.contains(3));

        owners.release(&selected);
        let required = BTreeSet::from([1usize]);
        assert!(
            owners
                .try_acquire_for_allocation(&required, &[(2, 128)], 1)
                .is_none()
        );
    }
}
