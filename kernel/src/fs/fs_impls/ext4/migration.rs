// SPDX-License-Identifier: MPL-2.0

//! Bounded asynchronous NV migration with queue-bound rTIO workers.
//! BIO callbacks enqueue only;
//! process-context completion owns durability and publishes reclaim work items;
//! foreground or background helpers own SSD publication and NVPage reclamation.
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aster_block::bio::{BioDirection, BioQueueAffinity};
use ostd::{
    cpu::CpuId,
    sync::{LocalIrqDisabled, RwMutexWriteGuard},
    util::id_set::Id,
};
use spin::Once;

use super::{block_group::AllocationGroupLease, fs::Ext4, pipeline_profile as pp, prelude::*};
use crate::fs::rnvpc::{
    self,
    baseline_generation::BaselineRangeToken,
    batch_metrics::{self, Event, Phase},
    frame::{ClaimedDirtyExtent, EvictingDataPage},
    metadata::PersistentObjectRef,
    redirect_index::{DescriptorHandle, NvExtentEntryLease},
    write_transaction::SsdBaselineExtent,
};

mod batch;
mod durability;
mod reclaim;
pub(crate) mod worker;

use batch::MigrationBatch;
pub(super) use durability::DurabilityArbiter;
use reclaim::ReclaimReadyQueue;
use worker::DurableBatchResult;
pub(super) use worker::RtioWorkerRuntime;

const MIGRATION_BATCH_PAGES: usize = super::writeback::WRITEBACK_BATCH;
const MAX_RTIO_WORKERS: usize = 16;

static RTIO_WORKERS_PARAM: Once<String> = Once::new();
aster_cmdline::define_kv_param!("rtio_workers", RTIO_WORKERS_PARAM);

pub(super) fn rtio_worker_count(
    mode: rnvpc::mode::RnvpcMode,
    _cpu_count: usize,
    submission_queues: u16,
    override_value: Option<usize>,
) -> Result<usize> {
    if override_value.is_some_and(|workers| !(1..=MAX_RTIO_WORKERS).contains(&workers)) {
        return_errno_with_message!(Errno::EINVAL, "rtio_workers must be between 1 and 16");
    }
    if mode != rnvpc::mode::RnvpcMode::RnvpcExtend {
        return Ok(1);
    }
    let submission_queues = usize::from(submission_queues);
    if submission_queues == 0 || override_value.is_some_and(|workers| workers > submission_queues) {
        return_errno_with_message!(
            Errno::EINVAL,
            "rtio_workers exceeds available submission queues"
        );
    }
    // The four-queue experiment regressed sequential writes; parallel workers
    // remain opt-in until the performance acceptance gates pass.
    Ok(override_value.unwrap_or(1))
}

pub(super) fn configured_rtio_worker_count(
    mode: rnvpc::mode::RnvpcMode,
    submission_queues: u16,
) -> Result<usize> {
    let override_value = RTIO_WORKERS_PARAM
        .get()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| Error::with_message(Errno::EINVAL, "rtio_workers must be an integer"))
        })
        .transpose()?;
    rtio_worker_count(
        mode,
        ostd::cpu::num_cpus(),
        submission_queues,
        override_value,
    )
}

const fn uses_shared_migration_rounds(mode: rnvpc::mode::RnvpcMode) -> bool {
    matches!(mode, rnvpc::mode::RnvpcMode::RnvpcExtend)
}

enum MigrationSubmitter {
    Worker(u16),
    Quiesced(u16),
    Legacy,
}

impl MigrationSubmitter {
    fn affinity(&self) -> BioQueueAffinity {
        match self {
            Self::Worker(queue_id) => BioQueueAffinity::Queue(*queue_id),
            Self::Quiesced(queue_id) => BioQueueAffinity::Queue(*queue_id),
            Self::Legacy => BioQueueAffinity::Any,
        }
    }
}

pub(super) struct MigrationGate {
    access: RwMutex<()>,
    exclusive_serial: Mutex<()>,
    quiescing: AtomicBool,
    shared_waiters: ostd::sync::WaitQueue,
}

impl MigrationGate {
    pub(super) const fn new() -> Self {
        Self {
            access: RwMutex::new(()),
            exclusive_serial: Mutex::new(()),
            quiescing: AtomicBool::new(false),
            shared_waiters: ostd::sync::WaitQueue::new(),
        }
    }

    pub(super) fn enter_shared(&self) -> MigrationPermit<'_> {
        self.shared_waiters.wait_until(|| {
            if self.quiescing.load(Ordering::Acquire) {
                return None;
            }
            let access = self.access.try_read()?;
            if self.quiescing.load(Ordering::Acquire) {
                drop(access);
                return None;
            }
            Some(MigrationPermit { _access: access })
        })
    }

    pub(super) fn enter_exclusive(&self) -> MigrationQuiesceGuard<'_> {
        let serial = self.exclusive_serial.lock();
        self.quiescing.store(true, Ordering::Release);
        let access = self.access.write();
        MigrationQuiesceGuard {
            gate: self,
            access: Some(access),
            _serial: serial,
        }
    }

    #[cfg(ktest)]
    fn is_quiescing_for_test(&self) -> bool {
        self.quiescing.load(Ordering::Acquire)
    }
}

pub(super) struct MigrationPermit<'a> {
    _access: RwMutexReadGuard<'a, ()>,
}

pub(super) struct MigrationQuiesceGuard<'a> {
    gate: &'a MigrationGate,
    access: Option<RwMutexWriteGuard<'a, ()>>,
    _serial: MutexGuard<'a, ()>,
}

impl Drop for MigrationQuiesceGuard<'_> {
    fn drop(&mut self) {
        drop(self.access.take());
        self.gate.quiescing.store(false, Ordering::Release);
        self.gate.shared_waiters.wake_all();
    }
}

pub(super) type CompletionQueue = Arc<SpinLock<VecDeque<Arc<MigrationBatch>>, LocalIrqDisabled>>;

pub(super) struct CompletionJob {
    status: BioStatus,
    pages: Vec<PendingPage>,
}
pub(super) type ReclaimQueue = Arc<ReclaimReadyQueue<ReclaimWorkItem>>;

pub(super) fn reclaim_queue() -> ReclaimQueue {
    Arc::new(ReclaimReadyQueue::new())
}

#[cfg(ktest)]
pub(super) fn assert_reclaim_queue_contract() {
    reclaim::assert_queue_contract();
}

pub(super) struct ReclaimWorkItem {
    entry: NvExtentEntryLease,
    batch: Arc<ReclaimBatch>,
    ready_at_ns: u64,
}

struct ReclaimBatch {
    remaining_work_items: AtomicUsize,
}

impl ReclaimBatch {
    fn new(work_items: usize) -> Arc<Self> {
        debug_assert!(work_items != 0);
        Arc::new(Self {
            remaining_work_items: AtomicUsize::new(work_items),
        })
    }

    fn complete_work_item(&self, fs: &Ext4) {
        if self.remaining_work_items.fetch_sub(1, Ordering::AcqRel) == 1 {
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

struct RedirectReclaimPlan {
    extent: Arc<ClaimedDirtyExtent>,
    pages: Vec<PendingPage>,
    exact: Vec<(rnvpc::index::PageKey, DescriptorHandle)>,
    new_baseline: Option<SsdBaselineExtent>,
    new_target_refs: Vec<PersistentObjectRef>,
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
    pub(super) fn release_unpublished_rnvpce_range(
        &self,
        range: Range<Ext4Bid>,
        nvm_allocated: bool,
    ) -> Result<()> {
        if nvm_allocated {
            return self
                .persist_rnvpce_free_before_release(range)
                .map_err(|failure| failure.into_error());
        }
        let op = self.begin_op(self.single_block_map_credits(0))?;
        self.free_blocks(
            super::journal::BlockFreeAuth::without_revoke_duty(
                range.start,
                u32::try_from(range.end - range.start).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            ),
            op.get(),
        )
    }

    fn rollback_unpublished_rnvpce_ranges(&self, ranges: &[Range<Ext4Bid>]) -> Result<()> {
        for range in ranges.iter().rev() {
            self.release_unpublished_rnvpce_range(range.clone(), true)?;
        }
        Ok(())
    }

    pub(super) fn start_nv_migration(self: &Arc<Self>) -> Result<()> {
        let worker_count = if uses_shared_migration_rounds(self.rnvpc_mode()) {
            self.rtio_profile.worker_count()
        } else {
            1
        };
        if uses_shared_migration_rounds(self.rnvpc_mode()) {
            pp::configure_rtio_workers(worker_count);
            rnvpc::pool()
                .ok_or_else(|| Error::new(Errno::EINVAL))?
                .configure_migration_workers(self.rnvpc_fs_id(), worker_count)?;
        }
        if !self.nv_migration_enabled() {
            return Ok(());
        }
        for worker_id in 0..worker_count {
            // Bind once before spawning; scheduler migration never changes the queue.
            let affinity = BioQueueAffinity::Queue(worker_id as u16);
            if uses_shared_migration_rounds(self.rnvpc_mode()) {
                self.rtio_profile
                    .record_worker_mapping(self.block_device().as_ref(), worker_id);
            }
            let weak = Arc::downgrade(self);
            let trigger = self.nv_migration_triggers[worker_id].clone();
            crate::thread::kernel_thread::ThreadOptions::new(move || {
                loop {
                    let observed;
                    let durability_generation;
                    {
                        let Some(fs) = weak.upgrade() else {
                            break;
                        };
                        observed = fs.nv_migration_worker_epochs[worker_id].load(Ordering::Acquire);
                        durability_generation = fs.rtio_durability.generation();
                        if uses_shared_migration_rounds(fs.rnvpc_mode()) {
                            if let Err(error) = fs.process_rtio_worker_completions(worker_id) {
                                error!("rTIO owner completion failed: {error:?}");
                                fs.fail_nv_migration();
                            }
                            match fs
                                .rtio_durability
                                .try_advance_background(&fs, Some(worker_id))
                            {
                                Ok(_) => {}
                                Err(error) => {
                                    error!("rTIO durability failed: {error:?}");
                                    fs.fail_nv_migration();
                                }
                            }
                            if let Err(error) = fs.process_rtio_worker_completions(worker_id) {
                                error!("rTIO owner completion failed: {error:?}");
                                fs.fail_nv_migration();
                            }
                        }
                        if fs.is_shutdown() || fs.nv_migration_failed.load(Ordering::Acquire) {
                            break;
                        }
                        if uses_shared_migration_rounds(fs.rnvpc_mode())
                            || rnvpc::needs_flush_watermark()
                            || fs.nv_capacity_waiters.load(Ordering::Acquire) != 0
                            || fs.nv_migration_progress_waiters.load(Ordering::Acquire) != 0
                        {
                            let result = if uses_shared_migration_rounds(fs.rnvpc_mode()) {
                                fs.migrate_nv_round_on_queue(MIGRATION_BATCH_PAGES, affinity)
                            } else {
                                fs.migrate_nv_round(MIGRATION_BATCH_PAGES)
                            };
                            match result {
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
                            match fs.rtio_durability.try_advance_idle(&fs, Some(worker_id)) {
                                Ok(processed) if processed != 0 => {
                                    if let Err(error) =
                                        fs.process_rtio_worker_completions(worker_id)
                                    {
                                        error!("rTIO owner completion failed: {error:?}");
                                        fs.fail_nv_migration();
                                    }
                                    continue;
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    error!("rTIO durability failed: {error:?}");
                                    fs.fail_nv_migration();
                                    break;
                                }
                            }
                        }
                    }
                    // The timeout only retries idle/stalled candidates. Successful
                    // rounds have no timer delay; completion wakes all workers.
                    let _ = trigger.wait_until_or_timeout(
                        || {
                            weak.upgrade()
                                .is_none_or(|fs| {
                                    fs.is_shutdown()
                                        || fs.nv_migration_worker_epochs[worker_id]
                                            .load(Ordering::Acquire)
                                            != observed
                                        || (uses_shared_migration_rounds(fs.rnvpc_mode())
                                            && (fs.rtio_durability.generation()
                                                != durability_generation
                                                || fs.rtio_durability.ready_batch_count() != 0))
                                })
                                .then_some(())
                        },
                        &Duration::from_millis(10),
                    );
                }
            })
            .spawn();
        }
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
        if !self.rnvpc_extend_enabled() {
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
        if self.allocation_metadata_pager_enabled() {
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
        Ok(())
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
        // This lock coordinates only the pager with sync's final allocation
        // export. rNVPCe data migration does not acquire it.
        let _allocation_export = self.nv_migration_lock.lock();
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
        for (epoch, trigger) in self
            .nv_migration_worker_epochs
            .iter()
            .zip(self.nv_migration_triggers.iter())
        {
            epoch.fetch_add(1, Ordering::Release);
            trigger.wake_one();
        }
    }

    pub(super) fn request_nv_migration_for_inode(&self, inode: u64) {
        let Some(pool) = rnvpc::pool() else {
            return;
        };
        let Some(worker_id) = pool.nv_entry_owner_worker(self.rnvpc_fs_id(), inode) else {
            return;
        };
        let Some((epoch, trigger)) = self
            .nv_migration_worker_epochs
            .get(worker_id)
            .zip(self.nv_migration_triggers.get(worker_id))
        else {
            return;
        };
        epoch.fetch_add(1, Ordering::Release);
        trigger.wake_one();
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
        if uses_shared_migration_rounds(self.rnvpc_mode()) {
            // Direct process-context callers use a valid, explicit queue zero.
            return self.migrate_nv_round_on_queue(budget, BioQueueAffinity::Queue(0));
        }
        let _serial = self.nv_migration_lock.lock();
        self.migrate_nv_round_inner(budget, MigrationSubmitter::Legacy)
    }

    pub(super) fn migrate_nv_round_on_queue(
        &self,
        budget: usize,
        affinity: BioQueueAffinity,
    ) -> Result<usize> {
        let BioQueueAffinity::Queue(queue_id) = affinity else {
            return_errno_with_message!(Errno::EINVAL, "rNVPCe migration requires a strict queue");
        };
        if !uses_shared_migration_rounds(self.rnvpc_mode())
            || queue_id >= self.block_device().metadata().nr_submission_queues
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "rNVPCe migration requires a valid strict queue"
            );
        }
        let _permit = self.nv_migration_gate.enter_shared();
        let _active_worker = pp::enter_rtio_worker_round();
        self.migrate_nv_round_inner(budget, MigrationSubmitter::Worker(queue_id))
    }

    fn migrate_rnvpce_round_quiesced(
        &self,
        guard: &MigrationQuiesceGuard<'_>,
        budget: usize,
    ) -> Result<usize> {
        if !core::ptr::eq(guard.gate, &self.nv_migration_gate) {
            return_errno_with_message!(Errno::EINVAL, "migration guard belongs to another fs");
        }
        // Exclusive quiescence owns this round; no worker/shared permit is taken.
        // Preserve logical owner and hardware-queue affinity while the sync
        // context temporarily performs the owner's submission work.
        let worker_count = rnvpc::pool()
            .and_then(|pool| pool.migration_worker_count(self.rnvpc_fs_id()))
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        for worker_id in 0..worker_count {
            let submitted = self.migrate_nv_round_inner(
                budget,
                MigrationSubmitter::Quiesced(
                    u16::try_from(worker_id).map_err(|_| Error::new(Errno::EOVERFLOW))?,
                ),
            )?;
            if submitted != 0 {
                return Ok(submitted);
            }
        }
        Ok(0)
    }

    fn migrate_nv_round_inner(
        &self,
        budget: usize,
        submitter: MigrationSubmitter,
    ) -> Result<usize> {
        let affinity = submitter.affinity();
        let profile_started = pp::now();
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
        pool.ensure_publication_certain()?;
        let candidate_started = pp::now();
        let mut pages = Vec::new();
        let mut nv_entries = Vec::new();
        if self.rnvpc_extend_enabled() {
            let worker_id = match submitter {
                MigrationSubmitter::Worker(queue_id) => usize::from(queue_id),
                MigrationSubmitter::Quiesced(queue_id) => usize::from(queue_id),
                MigrationSubmitter::Legacy => 0,
            };
            nv_entries =
                pool.claim_ready_nv_entries_for_worker(self.rnvpc_fs_id(), worker_id, budget)?;
            for lease in &nv_entries {
                batch_metrics::count(Event::CandidateRanges, 1);
                batch_metrics::count(
                    Event::CandidatePages,
                    usize::from(lease.entry().page_count()),
                );
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
        let selected_pages = if self.rnvpc_extend_enabled() {
            nv_entries
                .iter()
                .map(|lease| usize::from(lease.entry().page_count()))
                .sum()
        } else {
            pages.len()
        };
        pp::NV_CANDIDATE_SELECT.record_since(candidate_started, selected_pages);
        if selected_pages == 0 {
            return Ok(0);
        }
        let target_started = pp::now();
        let migration_leases = Vec::new();
        pp::NV_TARGET_PREPARE.record_since(target_started, selected_pages);
        let submitted = selected_pages;
        if submitted == 0 {
            debug_assert!(migration_leases.is_empty());
            return Ok(0);
        }
        let data_submit_started = pp::now();
        let max_segments = self
            .block_device()
            .metadata()
            .max_nr_segments_per_bio
            .max(1);
        let mut submissions = Vec::new();
        if self.rnvpc_extend_enabled() {
            for lease in &nv_entries {
                let entry = lease.entry();
                let page_count = usize::from(entry.page_count());
                let mut offset = 0usize;
                while offset < page_count {
                    let chunk = (page_count - offset).min(max_segments);
                    let target = entry
                        .ssd_block_range()
                        .start
                        .checked_add(offset as u64)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    let mut segments = Vec::with_capacity(chunk);
                    for page_offset in offset..offset + chunk {
                        segments.push(BioSegment::new_from_segment(
                            pool.data_segment_for_migration(lease, page_offset)?,
                            BioDirection::ToDevice,
                        ));
                    }
                    submissions.push((target, segments, Vec::new()));
                    offset += chunk;
                }
            }
        } else {
            pages.sort_by_key(|page| page.target);
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
                submissions.push((target, segments, run));
            }
        }
        let owner_worker_id = self.rnvpc_extend_enabled().then(|| match submitter {
            MigrationSubmitter::Worker(queue_id) | MigrationSubmitter::Quiesced(queue_id) => {
                usize::from(queue_id)
            }
            MigrationSubmitter::Legacy => 0,
        });
        let batch = MigrationBatch::new(
            submissions.len(),
            submitted,
            migration_leases,
            nv_entries,
            owner_worker_id,
        );
        let submitted_bios = submissions.len();
        let in_flight = self.nv_in_flight.fetch_add(submitted, Ordering::AcqRel) + submitted;
        if self.rnvpc_extend_enabled() {
            self.rtio_durability.note_submitted_pages(submitted);
            pp::record_rtio_submission(submitted, submitted_bios, in_flight);
        }
        let mut submit_error = None;
        for (target, segments, run) in submissions {
            let completed = self.nv_completion_queue.clone();
            let trigger = self.nv_completion_trigger.clone();
            let owner_runtime =
                owner_worker_id.and_then(|owner| self.rtio_worker_runtimes.get(owner).cloned());
            let durability = self.rtio_durability.clone();
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
                if let Some(runtime) = owner_runtime.as_ref() {
                    callback_batch.complete_data_for_owner(
                        CompletionJob { status, pages },
                        runtime,
                        &durability,
                    );
                } else {
                    callback_batch.complete_data(
                        CompletionJob { status, pages },
                        &completed,
                        &trigger,
                    );
                }
            });
            let mut io_batch = IoBatch::new();
            let pages = segments.iter().map(BioSegment::nblocks).sum();
            let result = if self.rnvpc_extend_enabled() {
                self.write_segments_on_queue_async(
                    target,
                    segments,
                    affinity,
                    Some(callback),
                    &mut io_batch,
                )
            } else {
                self.write_segments_async(target, segments, Some(callback), &mut io_batch)
            };
            if result.is_ok()
                && let MigrationSubmitter::Worker(queue_id) = submitter
            {
                self.rtio_profile.record_submission(queue_id, pages, 1);
            }
            if let Err(error) = result {
                // Enqueue failure drops the callback without running it. Recover
                // its pages from the shared slot and complete the batch once.
                let pages = callback_pages
                    .lock()
                    .take()
                    .expect("failed migration submission must retain its pages");
                let job = CompletionJob {
                    status: BioStatus::IoError,
                    pages,
                };
                if let Some(runtime) =
                    owner_worker_id.and_then(|owner| self.rtio_worker_runtimes.get(owner))
                {
                    batch.complete_data_for_owner(job, runtime, &self.rtio_durability);
                } else {
                    batch.complete_data(
                        job,
                        &self.nv_completion_queue,
                        &self.nv_completion_trigger,
                    );
                }
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

    fn reserve_nv_target_groups(&self, pages: &[PendingPage]) -> Result<Vec<AllocationGroupLease>> {
        let required = pages
            .iter()
            .filter_map(|page| (page.target != 0).then_some(page.target))
            .map(|target| self.block_group_index_for_block(target))
            .collect::<Result<BTreeSet<_>>>()?;
        let needed = pages.iter().filter(|page| page.target == 0).count();
        self.reserve_nv_allocation_groups(&required, needed)
    }

    fn reserve_nv_allocation_groups(
        &self,
        required: &BTreeSet<usize>,
        needed: usize,
    ) -> Result<Vec<AllocationGroupLease>> {
        let cpu = CpuId::current_racy().as_usize();
        let mut leases = Vec::new();
        let mut selected = BTreeSet::new();
        let mut available = 0usize;
        for &group in required {
            let lease = self.block_group(group).try_claim_migration()?;
            available =
                available.saturating_add(self.block_group(group).free_blocks_count() as usize);
            selected.insert(group);
            leases.push(lease);
        }
        let mut saw_busy = false;
        if available < needed {
            for group in
                self.allocation_group_selector
                    .candidates(cpu, required.first().copied(), 1)
            {
                if selected.contains(&group) {
                    continue;
                }
                match self.block_group(group).try_claim_migration() {
                    Ok(lease) => {
                        available = available
                            .saturating_add(self.block_group(group).free_blocks_count() as usize);
                        selected.insert(group);
                        leases.push(lease);
                        if available >= needed {
                            break;
                        }
                    }
                    Err(error) if error.error() == Errno::EBUSY => saw_busy = true,
                    Err(error) => return Err(error),
                }
            }
        }
        if available < needed {
            return Err(Error::new(if saw_busy {
                Errno::EBUSY
            } else {
                Errno::ENOSPC
            }));
        }
        Ok(leases)
    }

    /// Allocates first-write SSD homes before the foreground transaction
    /// publishes its redirects and baseline extent. Allocation metadata is
    /// NVM-authoritative before any file mapping can name the returned blocks.
    pub(super) fn allocate_rnvpce_foreground_blocks(
        &self,
        count: u32,
        goal: Option<Ext4Bid>,
    ) -> Result<Vec<Range<Ext4Bid>>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let group_started = pp::now();
        let cpu = CpuId::current_racy().as_usize();
        let goal_group = goal
            .map(|block| self.block_group_index_for_block(block))
            .transpose()?;
        pp::NV_FOREGROUND_GROUP_RESERVE.record_since(group_started, count as usize);
        let mut ranges = Vec::new();
        let allocation_result = (|| {
            let mut remaining = count;
            let mut next_goal = goal;
            while remaining != 0 {
                let mut allocated_range = None;
                let mut saw_busy = false;
                for group in self.allocation_group_selector.candidates(
                    cpu,
                    next_goal
                        .map(|block| self.block_group_index_for_block(block))
                        .transpose()?
                        .or(goal_group),
                    1,
                ) {
                    let op = self.begin_op(self.single_block_map_credits(0))?;
                    let block_started = pp::now();
                    let allowed = BTreeSet::from([group]);
                    let allocation_goal = next_goal
                        .filter(|block| {
                            self.block_group_index_for_block(*block).ok() == Some(group)
                        })
                        .unwrap_or_else(|| self.block_group(group).first_block());
                    match self.alloc_rnvpce_blocks_at_in_groups(
                        remaining,
                        allocation_goal,
                        &allowed,
                        op.get(),
                    ) {
                        Ok(range) => {
                            pp::NV_FOREGROUND_BLOCK_ALLOCATE
                                .record_since(block_started, (range.end - range.start) as usize);
                            allocated_range = Some((group, range));
                            break;
                        }
                        Err(error) if error.error() == Errno::EBUSY => {
                            super::allocator::metrics::GROUP_BUSY_RETRIES.record(1);
                            saw_busy = true;
                        }
                        Err(error) if error.error() == Errno::ENOSPC => {
                            super::allocator::metrics::SUMMARY_FALSE_POSITIVES.record(1);
                        }
                        Err(error) => return Err(error),
                    }
                }
                let (group, range) = allocated_range.ok_or_else(|| {
                    Error::new(if saw_busy {
                        Errno::EBUSY
                    } else {
                        Errno::ENOSPC
                    })
                })?;
                let allocated = u32::try_from(range.end - range.start)
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?;
                if allocated == 0 || allocated > remaining {
                    return_errno_with_message!(Errno::EUCLEAN, "invalid foreground allocation");
                }
                remaining -= allocated;
                next_goal = Some(range.end);
                self.allocation_group_selector.record_success(cpu, group);
                let metadata_started = pp::now();
                if let Err(failure) = self.persist_rnvpce_allocation_range(range.clone(), true) {
                    let uncertain = failure.is_uncertain();
                    let error = failure.into_error();
                    if !uncertain {
                        self.release_unpublished_rnvpce_range(range, false)?;
                    }
                    return Err(error);
                }
                pp::NV_FOREGROUND_ALLOCATION_METADATA
                    .record_since(metadata_started, allocated as usize);
                ranges.push(range);
            }
            Ok(())
        })();
        if let Err(error) = allocation_result {
            if let Err(rollback_error) = self.rollback_unpublished_rnvpce_ranges(&ranges) {
                self.fail_nv_migration();
                error!("foreground SSD allocation rollback failed: {rollback_error:?}");
            }
            return Err(error);
        }
        Ok(ranges)
    }

    /// Allocates SSD targets and publishes their identities in NVM.
    /// Group ownership precedes every bitmap/GDT mutation, and EXT4 allocation
    /// metadata joins the later orderless batch barrier.
    fn prepare_nv_targets(&self, pages: &mut [PendingPage]) -> Result<Vec<AllocationGroupLease>> {
        debug_assert!(
            !pages.is_empty(),
            "empty migration rounds must skip target preparation"
        );
        let group_reservation = self.reserve_nv_target_groups(pages)?;
        let groups = group_reservation
            .iter()
            .map(AllocationGroupLease::group)
            .collect::<BTreeSet<_>>();
        let mut allocated = Vec::with_capacity(pages.len());
        let mut durable_ranges = Vec::new();
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
                if let Err(failure) = self.persist_rnvpce_allocation_range(blocks.clone(), true) {
                    let uncertain = failure.is_uncertain();
                    let error = failure.into_error();
                    if !uncertain {
                        self.release_unpublished_rnvpce_range(blocks, false)?;
                    }
                    return Err(error);
                }
                durable_ranges.push(blocks.clone());
                for target in blocks {
                    allocated.push((cursor, target));
                    cursor += 1;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Err(rollback_error) = self.rollback_unpublished_rnvpce_ranges(&durable_ranges) {
                self.fail_nv_migration();
                error!("migration SSD allocation rollback failed: {rollback_error:?}");
            }
            return Err(error);
        }
        // Every allocated bitmap word is NVM-authoritative before any NV
        // descriptor can name its SSD target. No bitmap/GDT SSD I/O occurs.
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
            self.rollback_unpublished_rnvpce_ranges(&durable_ranges)?;
            return_errno!(Errno::EINVAL);
        };
        let started = batch_metrics::now();
        let targets = match targets {
            Ok(targets) => targets,
            Err(error) => {
                self.rollback_unpublished_rnvpce_ranges(&durable_ranges)?;
                return Err(error);
            }
        };
        for targets in targets.chunks(rnvpc::write_transaction::SSD_MIGRATION_BATCH_PAGES) {
            if let Err(error) = pool.assign_redirect_migration_targets(targets, None) {
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
        Ok(group_reservation)
    }

    pub(super) fn process_nv_completions(&self) -> Result<usize> {
        if self.rnvpc_extend_enabled() {
            let mut failure = self.rtio_durability.try_advance(self, None).err();
            let mut count = 0usize;
            for worker_id in 0..self.rtio_worker_runtimes.len() {
                match self.process_rtio_worker_completions(worker_id) {
                    Ok(processed) => count = count.saturating_add(processed),
                    Err(error) => failure = Some(error),
                }
            }
            return match failure {
                Some(error) => Err(error),
                None => Ok(count),
            };
        }
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
        match failure {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    fn process_rtio_worker_completions(&self, worker_id: usize) -> Result<usize> {
        let runtime = self
            .rtio_worker_runtimes
            .get(worker_id)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let data_complete_depth = runtime.data_complete_depth();
        let durable_result_depth = runtime.durable_result_depth();
        let mut count = 0usize;
        let mut batches = 0usize;
        let mut failure = None;
        for result in runtime.take_durable_results() {
            batches = batches.saturating_add(1);
            count = count.saturating_add(result.batch.page_count());
            if let Err(error) = self.finish_rnvpce_durable_result(worker_id, result) {
                failure = Some(error);
            }
        }
        if count != 0 {
            self.nv_in_flight.fetch_sub(count, Ordering::AcqRel);
            self.request_nv_migration();
        }
        self.rtio_profile.record_owner_completion(
            worker_id,
            count,
            batches,
            data_complete_depth,
            durable_result_depth,
        );
        match failure {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    fn finish_rnvpce_durable_result(
        &self,
        worker_id: usize,
        result: DurableBatchResult,
    ) -> Result<()> {
        if result.batch.owner_worker_id() != Some(worker_id) {
            return_errno_with_message!(Errno::ESTALE, "rTIO completion reached non-owner");
        }
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let completion_started = result.batch.completion_started();
        if !result.durable {
            self.restore_failed_migration_jobs(&pool, result.jobs);
            return_errno_with_message!(Errno::EIO, "orderless migration batch I/O failed");
        }
        drop(result.jobs);
        let entries = result.batch.take_nv_entry_leases();
        if entries.is_empty() {
            return_errno_with_message!(Errno::EUCLEAN, "rNVPCe completion lost entry leases");
        }
        drop(result.batch.take_migration_leases());
        let reclaim_batch = ReclaimBatch::new(entries.len());
        for entry in entries {
            let entry_pages = usize::from(entry.entry().page_count());
            pool.complete_nv_entry(&entry)?;
            self.nv_reclaim_pending
                .fetch_add(entry_pages, Ordering::AcqRel);
            self.nv_reclaim_queue.publish(
                ReclaimWorkItem {
                    entry,
                    batch: reclaim_batch.clone(),
                    ready_at_ns: pp::now(),
                },
                entry_pages,
            );
            if self.nv_capacity_waiters.load(Ordering::Acquire) <= 1 {
                self.nv_reclaim_trigger.wake_one();
            }
            pool.wake_one_data_capacity_waiter();
        }
        pp::NV_COMPLETION_TOTAL.record_since(completion_started, result.batch.page_count());
        Ok(())
    }

    fn finish_nv_completion_batch(&self, batch: Arc<MigrationBatch>) -> Result<()> {
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
        self.persist_nv_allocations()?;
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
        Ok(())
    }

    pub(super) fn help_nv_reclaim(&self, budget: usize) -> Result<usize> {
        let mut work_items = Vec::new();
        let mut page_total = 0usize;
        while page_total < budget {
            let Some((work_item, pages_count)) = self.nv_reclaim_queue.claim() else {
                break;
            };
            let ReclaimWorkItem {
                entry,
                batch,
                ready_at_ns,
            } = work_item;
            pp::NV_RECLAIM_READY_WAIT.record_since(ready_at_ns, pages_count);
            page_total = page_total.saturating_add(pages_count);
            work_items.push((entry, batch, pages_count));
        }
        if work_items.is_empty() {
            return Ok(0);
        }
        let publish_started = pp::now();
        let pool = rnvpc::pool().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let mut result = Ok(());
        for (entry, _, _) in &work_items {
            if let Err(error) = pool.reclaim_nv_entry(entry) {
                result = Err(error);
                break;
            }
        }
        pp::NV_EXTENT_PUBLISH.record_since(publish_started, page_total);
        pp::NV_RECLAIM_HELP.record_since(publish_started, page_total);
        let reclaimed_ranges = work_items.len();
        for (_, batch, pages_count) in work_items {
            self.nv_reclaim_pending
                .fetch_sub(pages_count, Ordering::AcqRel);
            batch.complete_work_item(self);
        }
        if let Err(error) = result {
            self.fail_nv_migration();
            return Err(error);
        }
        batch_metrics::count(Event::ReclaimedPages, page_total);
        batch_metrics::count(Event::ReclaimedSlotRanges, reclaimed_ranges);
        self.nv_reclaim_trigger.wake_all();
        Ok(page_total)
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
            if self.rnvpc_extend_enabled() {
                crate::thread::Thread::yield_now();
                continue;
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
        let migration = self.nv_migration_gate.enter_exclusive();
        loop {
            let (submitted, deferred) =
                match self.migrate_rnvpce_round_quiesced(&migration, MIGRATION_BATCH_PAGES) {
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
        // After the device flush, exact-generation transitions transfer
        // authority back to SSD and release the resident bitmap chunks.
        let _allocation_export = self.nv_migration_lock.lock();
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

    fn finish_redirect_page_batches(
        &self,
        pool: &Arc<rnvpc::frame::Rnvpc>,
        page_groups: Vec<Vec<PendingPage>>,
    ) -> Result<()> {
        let mut plans = Vec::with_capacity(page_groups.len());
        for mut pages in page_groups {
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
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "reclaim work item mixes migration extents"
                );
            }
            pages.sort_unstable_by_key(|page| page.index);
            let mut exact = Vec::with_capacity(pages.len());
            let mut new_target_refs = Vec::new();
            let mut baseline_valid = true;
            let compare_started = batch_metrics::now();
            let baseline_token = pages
                .first()
                .and_then(|page| page.redirect.as_ref().map(|(_, token)| token.clone()))
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            let generation_matches = pool.validate_baseline_token(&baseline_token);
            for page in &pages {
                let (handle, token) = page
                    .redirect
                    .as_ref()
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                if !Arc::ptr_eq(token, &baseline_token) {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "migration extent mixes baseline tokens"
                    );
                }
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
                    new_target_refs.push(handle.reference);
                }
            }
            batch_metrics::elapsed(Phase::CompletionCompare, compare_started);
            batch_metrics::count(Event::CompletionComparisons, exact.len());
            if !baseline_valid {
                drop(pages);
                continue;
            }
            let new_baseline = if new_target_refs.is_empty() {
                None
            } else {
                if new_target_refs.len() != pages.len() {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "migration extent mixes new and existing SSD baselines"
                    );
                }
                let physical_start = pages[0].target;
                if pages.iter().enumerate().any(|(offset, page)| {
                    page.index != extent.logical_start() + offset as u64
                        || physical_start.checked_add(offset as u64) != Some(page.target)
                }) {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "migration extent has a discontinuous SSD target"
                    );
                }
                Some(SsdBaselineExtent::new(
                    extent.logical_start(),
                    u32::try_from(pages.len()).map_err(|_| Error::new(Errno::EOVERFLOW))?,
                    physical_start,
                )?)
            };
            plans.push(RedirectReclaimPlan {
                extent,
                pages,
                exact,
                new_baseline,
                new_target_refs,
            });
        }

        let mut publication_sessions = BTreeMap::<u64, Vec<usize>>::new();
        for (index, plan) in plans.iter().enumerate() {
            if plan.new_baseline.is_some() {
                publication_sessions
                    .entry(plan.extent.inode())
                    .or_default()
                    .push(index);
            }
        }
        for (inode, plan_indexes) in publication_sessions {
            let mut baselines = plan_indexes
                .iter()
                .map(|index| plans[*index].new_baseline.unwrap())
                .collect::<Vec<_>>();
            baselines.sort_unstable_by_key(|baseline| baseline.logical_start());
            let page_count = plan_indexes
                .iter()
                .map(|index| plans[*index].pages.len())
                .sum();
            let baseline_started = pp::now();
            rnvpc::write_transaction::publish_claimed_ssd_baseline_extents(
                pool,
                self.rnvpc_fs_id(),
                inode,
                &baselines,
            )?;
            pp::NV_EXTENT_PREPARE.record_since(baseline_started, page_count);
            let mut releasable = Vec::with_capacity(page_count);
            for plan_index in plan_indexes {
                for reference in &plans[plan_index].new_target_refs {
                    if !pool.persistent_data_has_migration_target(*reference)? {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "published SSD baseline has no migration target"
                        );
                    }
                    if !pool.persistent_data_migration_credit_released(*reference)? {
                        releasable.push(*reference);
                    }
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

        for plan in plans {
            plan.extent.release_physical_for_completion()?;
            for chunk in plan.exact.chunks(128) {
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
            plan.extent.complete_success()?;
            drop(plan.pages);
        }
        Ok(())
    }
}

#[cfg(ktest)]
pub(super) mod tests {
    use ostd::prelude::ktest;

    use super::{
        super::nv_tests::{
            QueueRecordingDisk, assert_cross_queue_ssd_page, cross_queue_fixture, cross_queue_write,
        },
        *,
    };
    use crate::fs::{
        fs_impls::ext4::{
            allocator::group_selector::AllocationGroupSelector,
            block_group::AllocationWritebackState,
        },
        rnvpc::mode::RnvpcMode,
    };

    #[ktest]
    fn rtio_worker_policy_defaults_and_bounds() {
        for (cpus, queues, expected) in [
            (64, 16, 1),
            (16, 16, 1),
            (4, 16, 1),
            (0, 16, 1),
            (64, 2, 1),
            (64, 1, 1),
            (256, 16, 1),
        ] {
            assert_eq!(
                rtio_worker_count(RnvpcMode::RnvpcExtend, cpus, queues, None).unwrap(),
                expected
            );
        }
        for workers in [1, 2, 4, 16] {
            assert_eq!(
                rtio_worker_count(RnvpcMode::RnvpcExtend, 64, 16, Some(workers)).unwrap(),
                workers
            );
        }
        for (queues, workers) in [(1, 1), (2, 2), (4, 4), (16, 16)] {
            assert_eq!(
                rtio_worker_count(RnvpcMode::RnvpcExtend, 64, queues, Some(workers)).unwrap(),
                workers
            );
        }
        for (queues, workers) in [(16, 0), (16, 17), (2, 3), (1, 4)] {
            assert_eq!(
                rtio_worker_count(RnvpcMode::RnvpcExtend, 64, queues, Some(workers))
                    .unwrap_err()
                    .error(),
                Errno::EINVAL
            );
        }
        assert_eq!(
            rtio_worker_count(RnvpcMode::RnvpcExtend, 64, 0, None)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        for mode in [RnvpcMode::Rnvpc, RnvpcMode::Native] {
            assert_eq!(rtio_worker_count(mode, 64, 1, None).unwrap(), 1);
            assert_eq!(rtio_worker_count(mode, 64, 1, Some(4)).unwrap(), 1);
        }
    }

    #[ktest]
    fn rnvpce_workers_keep_stable_distinct_queue_ids() {
        use aster_block::bio::BioQueueAffinity;
        use ostd::mm::VmReader;

        use super::super::test_utils::{Ext4FixtureBuilder, make_empty_file_inode};
        use crate::fs::vfs::inode::FileOps;

        rnvpc::init_for_ext4_tests();
        crate::time::clocks::init_for_ktest();
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(128)
            .with_uuid([0xe4; 16])
            .build()
            .unwrap();
        for ino in 11..20 {
            fixture.write_raw_inode(ino, &make_empty_file_inode());
        }
        assert_eq!(
            fixture
                .ext4
                .migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(0))
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        let disk = Arc::new(QueueRecordingDisk::new(fixture.disk.clone()));
        drop(fixture);
        let options = CString::new("rnvpce=on,rtio=off").unwrap();
        let fs = Ext4::open(disk.clone(), Some(options.as_c_str())).unwrap();
        rnvpc::pool()
            .unwrap()
            .configure_migration_workers(fs.rnvpc_fs_id(), 4)
            .unwrap();
        for ino in 11..19 {
            fs.read_inode(ino)
                .unwrap()
                .write_at(
                    0,
                    &mut VmReader::from([ino as u8; PAGE_SIZE].as_slice()).to_fallible(),
                )
                .unwrap();
        }
        disk.writes.lock().clear();
        for affinity in [BioQueueAffinity::Any, BioQueueAffinity::Queue(4)] {
            assert_eq!(
                fs.migrate_nv_round_on_queue(1, affinity)
                    .unwrap_err()
                    .error(),
                Errno::EINVAL
            );
        }
        let mut workers = Vec::new();
        for worker_id in 0..4 {
            let fs = fs.clone();
            workers.push(
                crate::thread::kernel_thread::ThreadOptions::new(move || {
                    let affinity = BioQueueAffinity::Queue(worker_id);
                    for _ in 0..2 {
                        assert_eq!(fs.migrate_nv_round_on_queue(1, affinity).unwrap(), 1);
                        crate::thread::Thread::yield_now();
                    }
                })
                .spawn(),
            );
        }
        for worker in workers {
            worker.join();
        }
        let writes = disk.writes.lock();
        assert_eq!(writes.len(), 8);
        for queue_id in 0..4 {
            assert_eq!(
                writes
                    .iter()
                    .filter(|&&affinity| affinity == BioQueueAffinity::Queue(queue_id))
                    .count(),
                2
            );
        }
        drop(writes);
        for queue_id in 0..4 {
            let stats = fs.rtio_profile.submission_snapshot(queue_id).unwrap();
            assert_eq!(stats.pages, 2);
            assert_eq!(stats.bios, 2);
        }
        let before = disk.disk.flush_count();
        assert_eq!(fs.process_nv_completions().unwrap(), 8);
        assert_eq!(disk.disk.flush_count() - before, 1);
        assert_eq!(fs.help_nv_reclaim(8).unwrap(), 8);

        fs.read_inode(19)
            .unwrap()
            .write_at(
                0,
                &mut VmReader::from([19; PAGE_SIZE].as_slice()).to_fallible(),
            )
            .unwrap();
        disk.writes.lock().clear();
        let guard = fs.nv_migration_gate.enter_exclusive();
        assert_eq!(fs.migrate_rnvpce_round_quiesced(&guard, 1).unwrap(), 1);
        drop(guard);
        assert_eq!(*disk.writes.lock(), vec![BioQueueAffinity::Queue(0)]);
        assert_eq!(
            fs.rtio_profile.submission_snapshot(0).unwrap().pages,
            2,
            "quiesced sync submissions are not worker activity"
        );
        assert_eq!(fs.process_nv_completions().unwrap(), 1);
        assert_eq!(fs.help_nv_reclaim(1).unwrap(), 1);
        disk.writes.lock().clear();
        fs.write_segments_async(
            200,
            vec![BioSegment::alloc(1, BioDirection::ToDevice)],
            None,
            &mut IoBatch::new(),
        )
        .unwrap();
        assert_eq!(*disk.writes.lock(), vec![BioQueueAffinity::Any]);
    }

    fn wait_for_queue_test(mut condition: impl FnMut() -> bool) {
        let deadline = ostd::timer::Jiffies::elapsed().as_duration() + Duration::from_secs(5);
        while !condition() {
            assert!(
                ostd::timer::Jiffies::elapsed().as_duration() < deadline,
                "queue test progress timed out"
            );
            crate::thread::Thread::yield_now();
        }
    }

    #[ktest]
    fn rnvpce_ready_owners_share_one_durability_flush() {
        let (fs, disk) = cross_queue_fixture(0xed);
        let batch0 = MigrationBatch::new(1, 1, Vec::new(), Vec::new(), Some(0));
        let batch1 = MigrationBatch::new(1, 1, Vec::new(), Vec::new(), Some(1));
        fs.rtio_durability.note_submitted_pages(2);
        batch0.complete_data_for_owner(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &fs.rtio_worker_runtimes[0],
            &fs.rtio_durability,
        );
        batch1.complete_data_for_owner(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &fs.rtio_worker_runtimes[1],
            &fs.rtio_durability,
        );

        let before = disk.disk.flush_count();
        assert_eq!(fs.rtio_durability.try_advance(&fs, None).unwrap(), 2);
        assert_eq!(disk.disk.flush_count() - before, 1);
        assert_eq!(fs.rtio_durability.ready_page_count(), 0);
        assert_eq!(fs.rtio_worker_runtimes[0].durable_result_depth(), 1);
        assert_eq!(fs.rtio_worker_runtimes[1].durable_result_depth(), 1);
        assert_eq!(fs.rtio_worker_runtimes[2].durable_result_depth(), 0);
        assert_eq!(fs.rtio_worker_runtimes[3].durable_result_depth(), 0);

        let late = MigrationBatch::new(1, 1, Vec::new(), Vec::new(), Some(2));
        fs.rtio_durability.note_submitted_pages(1);
        late.complete_data_for_owner(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &fs.rtio_worker_runtimes[2],
            &fs.rtio_durability,
        );
        assert_eq!(fs.rtio_durability.try_advance(&fs, None).unwrap(), 1);
        assert_eq!(disk.disk.flush_count() - before, 2);
        assert_eq!(fs.rtio_worker_runtimes[2].durable_result_depth(), 1);
    }

    fn publish_test_data_complete_batch(fs: &Ext4, owner: usize, pages: usize) {
        let batch = MigrationBatch::new(1, pages, Vec::new(), Vec::new(), Some(owner));
        batch.complete_data_for_owner(
            CompletionJob {
                status: BioStatus::Complete,
                pages: Vec::new(),
            },
            &fs.rtio_worker_runtimes[owner],
            &fs.rtio_durability,
        );
    }

    #[ktest]
    fn rnvpce_durability_coalescing_defers_small_epoch_with_pending_data() {
        let (fs, disk) = cross_queue_fixture(0xf1);
        fs.rtio_durability.note_submitted_pages(2);
        publish_test_data_complete_batch(&fs, 0, 1);

        let before = disk.disk.flush_count();
        assert_eq!(
            fs.rtio_durability
                .try_advance_background(&fs, Some(0))
                .unwrap(),
            0
        );
        assert_eq!(disk.disk.flush_count(), before);
        assert_eq!(fs.rtio_durability.pending_data_page_count(), 1);
        assert_eq!(fs.rtio_durability.ready_page_count(), 1);
        assert_eq!(fs.rtio_worker_runtimes[0].data_complete_depth(), 1);
    }

    #[ktest]
    fn rnvpce_durability_busy_worker_does_not_mistake_no_pending_bio_for_idle() {
        let (fs, disk) = cross_queue_fixture(0xf5);
        fs.rtio_durability.note_submitted_pages(1);
        publish_test_data_complete_batch(&fs, 0, 1);

        let before = disk.disk.flush_count();
        assert_eq!(
            fs.rtio_durability
                .try_advance_background(&fs, Some(0))
                .unwrap(),
            0
        );
        assert_eq!(disk.disk.flush_count(), before);
        assert_eq!(fs.rtio_durability.ready_page_count(), 1);
    }

    #[ktest]
    fn rnvpce_durability_coalescing_flushes_tail_without_pending_data() {
        let (fs, disk) = cross_queue_fixture(0xf2);
        fs.rtio_durability.note_submitted_pages(1);
        publish_test_data_complete_batch(&fs, 0, 1);

        let before = disk.disk.flush_count();
        assert_eq!(
            fs.rtio_durability.try_advance_idle(&fs, Some(0)).unwrap(),
            1
        );
        assert_eq!(disk.disk.flush_count() - before, 1);
        assert_eq!(fs.rtio_durability.ready_page_count(), 0);
    }

    #[ktest]
    fn rnvpce_durability_coalescing_flushes_full_epoch_with_pending_data() {
        let (fs, disk) = cross_queue_fixture(0xf3);
        fs.rtio_durability
            .note_submitted_pages(MIGRATION_BATCH_PAGES + 1);
        publish_test_data_complete_batch(&fs, 0, MIGRATION_BATCH_PAGES);

        let before = disk.disk.flush_count();
        assert_eq!(
            fs.rtio_durability
                .try_advance_background(&fs, Some(0))
                .unwrap(),
            MIGRATION_BATCH_PAGES
        );
        assert_eq!(disk.disk.flush_count() - before, 1);
        assert_eq!(fs.rtio_durability.pending_data_page_count(), 1);
        assert_eq!(fs.rtio_durability.ready_page_count(), 0);
    }

    #[ktest]
    fn rnvpce_durability_force_advance_ignores_coalescing_threshold() {
        let (fs, disk) = cross_queue_fixture(0xf4);
        fs.rtio_durability.note_submitted_pages(2);
        publish_test_data_complete_batch(&fs, 0, 1);

        let before = disk.disk.flush_count();
        assert_eq!(fs.rtio_durability.try_advance(&fs, None).unwrap(), 1);
        assert_eq!(disk.disk.flush_count() - before, 1);
        assert_eq!(fs.rtio_durability.pending_data_page_count(), 1);
        assert_eq!(fs.rtio_durability.ready_page_count(), 0);
    }

    // Catches sync passing active workers, admitting a late worker while
    // quiescing, failing to drain, or taking a shared permit under exclusivity.
    #[ktest]
    pub(in crate::fs::fs_impls::ext4) fn rnvpce_sync_quiesces_all_queue_bound_workers() {
        use crate::fs::rnvpc::{index::PageKey, redirect_index::NvExtentEntryState};

        let (fs, disk) = cross_queue_fixture(0xe8);
        for ino in 11..14 {
            cross_queue_write(&fs, ino);
        }
        disk.writes.lock().clear();
        disk.pause_writes.store(0b11, Ordering::Release);
        let mut workers = Vec::new();
        for queue in 0..2 {
            let fs = fs.clone();
            workers.push(
                crate::thread::kernel_thread::ThreadOptions::new(move || {
                    assert_eq!(
                        fs.migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(queue))
                            .unwrap(),
                        1
                    );
                })
                .spawn(),
            );
        }
        wait_for_queue_test(|| disk.entered_writes.load(Ordering::Acquire) == 0b11);
        assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 2);
        assert!(fs.nv_completion_queue.lock().is_empty());
        let sync_done = Arc::new(AtomicBool::new(false));
        disk.pause_flush.store(true, Ordering::Release);
        let sync_fs = fs.clone();
        let sync_done_for_thread = sync_done.clone();
        let sync = crate::thread::kernel_thread::ThreadOptions::new(move || {
            sync_fs.export_nv_for_sync().unwrap();
            sync_done_for_thread.store(true, Ordering::Release);
        })
        .spawn();
        wait_for_queue_test(|| fs.nv_migration_gate.is_quiescing_for_test());

        let late_state = Arc::new(AtomicUsize::new(0));
        let late_fs = fs.clone();
        let late_state_for_thread = late_state.clone();
        let late_worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
            late_state_for_thread.store(1, Ordering::Release);
            assert_eq!(
                late_fs
                    .migrate_nv_round_on_queue(1, BioQueueAffinity::Queue(2))
                    .unwrap(),
                0
            );
            late_state_for_thread.store(2, Ordering::Release);
        })
        .spawn();
        wait_for_queue_test(|| late_state.load(Ordering::Acquire) == 1);

        disk.released_writes.store(0b01, Ordering::Release);
        disk.waiters.wake_all();
        workers.remove(0).join();
        // One active shared round still owns queue 1. Sync cannot reach its
        // data barrier even though queue 0's callback has already completed.
        assert!(!sync_done.load(Ordering::Acquire));
        assert!(!disk.entered_flush.load(Ordering::Acquire));
        assert_eq!(late_state.load(Ordering::Acquire), 1);
        assert!(fs.nv_completion_queue.lock().is_empty());
        assert_eq!(fs.rtio_worker_runtimes[0].data_complete_depth(), 1);
        assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 2);

        disk.released_writes.store(0b11, Ordering::Release);
        disk.waiters.wake_all();
        workers.pop().unwrap().join();
        wait_for_queue_test(|| disk.entered_flush.load(Ordering::Acquire));
        // Exclusive sync has submitted the third dirty page itself on its
        // owner queue 2
        // and is draining all three pages through the common durability gate.
        assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 3);
        assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
        assert_eq!(fs.nv_reclaim_queue.ready_units(), 0);
        assert!(!sync_done.load(Ordering::Acquire));
        assert_eq!(late_state.load(Ordering::Acquire), 1);
        assert!(fs.nv_migration_gate.is_quiescing_for_test());
        let writes = disk.writes.lock();
        assert_eq!(writes.len(), 3);
        assert_eq!(
            writes
                .iter()
                .filter(|&&queue| queue == BioQueueAffinity::Queue(0))
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|&&queue| queue == BioQueueAffinity::Queue(1))
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|&&queue| queue == BioQueueAffinity::Queue(2))
                .count(),
            1
        );
        drop(writes);
        let pool = rnvpc::pool().unwrap();
        for ino in 11..14 {
            assert_eq!(
                pool.lookup_dirty_nv_entry(PageKey::new(fs.rnvpc_fs_id(), ino, 0))
                    .unwrap()
                    .state(),
                NvExtentEntryState::Migrating
            );
        }
        disk.pause_flush.store(false, Ordering::Release);
        disk.waiters.wake_all();
        wait_for_queue_test(|| {
            sync_done.load(Ordering::Acquire) && late_state.load(Ordering::Acquire) == 2
        });
        sync.join();
        late_worker.join();
        assert!(!fs.nv_migration_gate.is_quiescing_for_test());
        assert_eq!(fs.nv_in_flight.load(Ordering::Acquire), 0);
        assert_eq!(fs.nv_reclaim_pending.load(Ordering::Acquire), 0);
        assert!(fs.nv_completion_queue.lock().is_empty());
        assert_eq!(fs.rtio_durability.ready_batch_count(), 0);
        for runtime in fs.rtio_worker_runtimes.iter() {
            assert_eq!(runtime.data_complete_depth(), 0);
            assert_eq!(runtime.durable_result_depth(), 0);
        }
        assert_eq!(fs.nv_reclaim_queue.ready_units(), 0);
        for ino in 11..14 {
            assert_cross_queue_ssd_page(&fs, &disk, ino);
        }
        assert_eq!(fs.process_nv_completions().unwrap(), 0);
        assert_eq!(fs.help_nv_reclaim(3).unwrap(), 0);
    }

    #[ktest]
    fn only_rnvpce_uses_parallel_migration_rounds() {
        assert!(uses_shared_migration_rounds(RnvpcMode::RnvpcExtend));
        assert!(!uses_shared_migration_rounds(RnvpcMode::Rnvpc));
        assert!(!uses_shared_migration_rounds(RnvpcMode::Native));
    }

    #[ktest]
    fn migration_gate_shared_rounds_precede_quiesce() {
        let gate = Arc::new(MigrationGate::new());
        let first = gate.enter_shared();
        let second = gate.enter_shared();
        let writer_state = Arc::new(AtomicUsize::new(0));
        let release_writer = Arc::new(AtomicBool::new(false));

        let writer_gate = gate.clone();
        let writer_state_for_thread = writer_state.clone();
        let release_writer_for_thread = release_writer.clone();
        let writer = crate::thread::kernel_thread::ThreadOptions::new(move || {
            writer_state_for_thread.store(1, Ordering::Release);
            let exclusive = writer_gate.enter_exclusive();
            writer_state_for_thread.store(2, Ordering::Release);
            while !release_writer_for_thread.load(Ordering::Acquire) {
                crate::thread::Thread::yield_now();
            }
            drop(exclusive);
            writer_state_for_thread.store(3, Ordering::Release);
        })
        .spawn();
        while writer_state.load(Ordering::Acquire) == 0 {
            crate::thread::Thread::yield_now();
        }
        while !gate.is_quiescing_for_test() {
            crate::thread::Thread::yield_now();
        }

        let late_reader_entered = Arc::new(AtomicBool::new(false));
        let late_gate = gate.clone();
        let late_reader_entered_for_thread = late_reader_entered.clone();
        let late_reader = crate::thread::kernel_thread::ThreadOptions::new(move || {
            let _shared = late_gate.enter_shared();
            late_reader_entered_for_thread.store(true, Ordering::Release);
        })
        .spawn();

        drop(first);
        crate::thread::Thread::yield_now();
        assert_eq!(writer_state.load(Ordering::Acquire), 1);
        drop(second);
        while writer_state.load(Ordering::Acquire) == 1 {
            crate::thread::Thread::yield_now();
        }
        assert_eq!(writer_state.load(Ordering::Acquire), 2);
        assert!(
            !late_reader_entered.load(Ordering::Acquire),
            "a queued exclusive guard must prevent later shared admission"
        );

        release_writer.store(true, Ordering::Release);
        writer.join();
        late_reader.join();
        assert_eq!(writer_state.load(Ordering::Acquire), 3);
        assert!(late_reader_entered.load(Ordering::Acquire));
    }

    #[ktest]
    fn rnvpce_group_lease_disjoint_groups_progress_concurrently() {
        let first = AllocationGroupLease::try_claim(
            1,
            Arc::new(Mutex::new(AllocationWritebackState::new_clean())),
        )
        .unwrap();
        let second = AllocationGroupLease::try_claim(
            2,
            Arc::new(Mutex::new(AllocationWritebackState::new_clean())),
        )
        .unwrap();
        assert_ne!(first.group(), second.group());
    }

    #[ktest]
    fn rnvpce_group_lease_same_group_has_one_migration_owner() {
        let state = Arc::new(Mutex::new(AllocationWritebackState::new_clean()));
        let first = AllocationGroupLease::try_claim(1, state.clone()).unwrap();
        assert!(AllocationGroupLease::try_claim(1, state.clone()).is_err());
        drop(first);
        assert!(AllocationGroupLease::try_claim(1, state).is_ok());
    }

    #[ktest]
    fn rnvpce_group_lease_stale_lease_cannot_release_new_owner() {
        let state = Arc::new(Mutex::new(AllocationWritebackState::new_clean()));
        let first = AllocationGroupLease::try_claim(1, state.clone()).unwrap();
        let stale_generation = first.generation();
        drop(first);
        let second = AllocationGroupLease::try_claim(1, state.clone()).unwrap();
        assert_ne!(stale_generation, second.generation());
        state.lock().release_migration(stale_generation);
        assert!(state.lock().has_migration_owner());
    }

    #[ktest]
    fn rnvpce_group_lease_writeback_defers_owned_group() {
        let state = Arc::new(Mutex::new(AllocationWritebackState::new_clean()));
        let _lease = AllocationGroupLease::try_claim(1, state.clone()).unwrap();
        assert!(state.lock().begin().is_none());
    }

    #[ktest]
    fn rnvpce_group_lease_busy_goal_retries_another_candidate() {
        let selector = AllocationGroupSelector::new(4, 2);
        selector.update_summary(0, 32, 32);
        selector.update_summary(2, 32, 32);
        let states = (0..4)
            .map(|_| Arc::new(Mutex::new(AllocationWritebackState::new_clean())))
            .collect::<Vec<_>>();
        let _busy = AllocationGroupLease::try_claim(0, states[0].clone()).unwrap();
        let selected = selector
            .candidates(0, Some(0), 1)
            .find_map(|group| AllocationGroupLease::try_claim(group, states[group].clone()).ok())
            .unwrap();
        assert_eq!(selected.group(), 2);
    }
}
