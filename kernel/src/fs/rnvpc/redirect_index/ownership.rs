// SPDX-License-Identifier: MPL-2.0

//! Volatile inode ownership for rNVPCe migration workers.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::{DirtyNvExtentEntry, InodeRedirectMap};
use crate::prelude::*;

pub(super) const INODE_WORK_IDLE: u8 = 0;
const INODE_WORK_QUEUED: u8 = 1;
const INODE_WORK_RUNNING: u8 = 2;
const INODE_WORK_RERUN: u8 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MigrationWorkerLoadSnapshot {
    pub(crate) queued_dirty_pages: usize,
    pub(crate) in_flight_pages: usize,
    pub(crate) completion_pending_pages: usize,
    pub(crate) owned_inodes: usize,
    pub(crate) ready_inode_dequeues: usize,
    pub(crate) ready_inode_empty: usize,
    pub(crate) ready_inode_invalid: usize,
    pub(crate) entry_state_conflicts: usize,
}

#[derive(Default)]
pub(super) struct InodeMigrationState {
    pub(super) owner: Option<usize>,
    pub(super) generation: u64,
    pub(super) active_entries: usize,
    pub(super) in_flight_pages: usize,
    pub(super) completion_pending_pages: usize,
    pub(super) reclaim_pending_pages: usize,
}

struct MigrationWorkerState {
    ready_inodes: SpinLock<VecDeque<Arc<InodeRedirectMap>>>,
    queued_dirty_pages: AtomicUsize,
    in_flight_pages: AtomicUsize,
    completion_pending_pages: AtomicUsize,
    owned_inodes: AtomicUsize,
    ready_inode_dequeues: AtomicUsize,
    ready_inode_empty: AtomicUsize,
    ready_inode_invalid: AtomicUsize,
    entry_state_conflicts: AtomicUsize,
}

impl MigrationWorkerState {
    fn new() -> Self {
        Self {
            ready_inodes: SpinLock::new(VecDeque::new()),
            queued_dirty_pages: AtomicUsize::new(0),
            in_flight_pages: AtomicUsize::new(0),
            completion_pending_pages: AtomicUsize::new(0),
            owned_inodes: AtomicUsize::new(0),
            ready_inode_dequeues: AtomicUsize::new(0),
            ready_inode_empty: AtomicUsize::new(0),
            ready_inode_invalid: AtomicUsize::new(0),
            entry_state_conflicts: AtomicUsize::new(0),
        }
    }

    fn load(&self) -> usize {
        self.queued_dirty_pages.load(Ordering::Acquire)
            + self.in_flight_pages.load(Ordering::Acquire)
            + self.completion_pending_pages.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> MigrationWorkerLoadSnapshot {
        MigrationWorkerLoadSnapshot {
            queued_dirty_pages: self.queued_dirty_pages.load(Ordering::Acquire),
            in_flight_pages: self.in_flight_pages.load(Ordering::Acquire),
            completion_pending_pages: self.completion_pending_pages.load(Ordering::Acquire),
            owned_inodes: self.owned_inodes.load(Ordering::Acquire),
            ready_inode_dequeues: self.ready_inode_dequeues.load(Ordering::Acquire),
            ready_inode_empty: self.ready_inode_empty.load(Ordering::Acquire),
            ready_inode_invalid: self.ready_inode_invalid.load(Ordering::Acquire),
            entry_state_conflicts: self.entry_state_conflicts.load(Ordering::Acquire),
        }
    }
}

pub(super) struct MigrationWorkerSet {
    workers: Box<[MigrationWorkerState]>,
    tie_break: AtomicUsize,
}

impl MigrationWorkerSet {
    const NO_OWNER: usize = usize::MAX;

    #[inline]
    fn record_migration_lock(inode_map: &InodeRedirectMap) {
        #[cfg(ktest)]
        inode_map
            .migration_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn new(worker_count: usize) -> Result<Self> {
        if worker_count == 0 {
            return_errno_with_message!(Errno::EINVAL, "migration worker count is zero");
        }
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(worker_count)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..worker_count {
            workers.push(MigrationWorkerState::new());
        }
        Ok(Self {
            workers: workers.into_boxed_slice(),
            tie_break: AtomicUsize::new(0),
        })
    }

    pub(super) fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub(super) fn queued_dirty_pages(&self) -> usize {
        self.workers
            .iter()
            .map(|worker| worker.queued_dirty_pages.load(Ordering::Acquire))
            .sum()
    }

    pub(super) fn is_idle(&self) -> bool {
        self.workers.iter().all(|worker| {
            worker.owned_inodes.load(Ordering::Acquire) == 0
                && worker.queued_dirty_pages.load(Ordering::Acquire) == 0
                && worker.in_flight_pages.load(Ordering::Acquire) == 0
                && worker.completion_pending_pages.load(Ordering::Acquire) == 0
                && worker.ready_inodes.lock().is_empty()
        })
    }

    fn choose_worker(&self) -> usize {
        let start = self.tie_break.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut selected = start;
        let mut selected_load = self.workers[start].load();
        for offset in 1..self.workers.len() {
            let worker = (start + offset) % self.workers.len();
            let load = self.workers[worker].load();
            if load < selected_load {
                selected = worker;
                selected_load = load;
            }
        }
        selected
    }

    fn schedule_inode(&self, inode_map: &Arc<InodeRedirectMap>, owner: usize) {
        loop {
            match inode_map.work_state.load(Ordering::Acquire) {
                INODE_WORK_IDLE => {
                    if inode_map
                        .work_state
                        .compare_exchange(
                            INODE_WORK_IDLE,
                            INODE_WORK_QUEUED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.workers[owner]
                            .ready_inodes
                            .lock()
                            .push_back(inode_map.clone());
                        return;
                    }
                }
                INODE_WORK_QUEUED | INODE_WORK_RERUN => return,
                INODE_WORK_RUNNING => {
                    if inode_map
                        .work_state
                        .compare_exchange(
                            INODE_WORK_RUNNING,
                            INODE_WORK_RERUN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                _ => unreachable!("invalid inode migration work state"),
            }
        }
    }

    pub(super) fn publish_dirty(
        self: &Arc<Self>,
        inode_map: &Arc<InodeRedirectMap>,
        entry: &Arc<DirtyNvExtentEntry>,
        pages: usize,
    ) {
        let owner = {
            Self::record_migration_lock(inode_map);
            let mut migration = inode_map.migration.lock();
            let owner = match migration.owner {
                Some(owner) => owner,
                None => {
                    let owner = self.choose_worker();
                    migration.owner = Some(owner);
                    migration.generation = migration.generation.wrapping_add(1);
                    self.workers[owner]
                        .owned_inodes
                        .fetch_add(1, Ordering::AcqRel);
                    inode_map.migration_owner.store(owner, Ordering::Release);
                    owner
                }
            };
            migration.active_entries = migration.active_entries.saturating_add(1);
            inode_map
                .queued_dirty_pages
                .fetch_add(pages, Ordering::AcqRel);
            self.workers[owner]
                .queued_dirty_pages
                .fetch_add(pages, Ordering::AcqRel);
            owner
        };
        inode_map.enqueue_dirty_entry(entry);
        self.schedule_inode(inode_map, owner);
    }

    pub(super) fn account_dirty_extension(&self, inode_map: &InodeRedirectMap, pages: usize) {
        Self::record_migration_lock(inode_map);
        let migration = inode_map.migration.lock();
        let owner = migration
            .owner
            .expect("a registered dirty extent has a migration owner");
        inode_map
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
        self.workers[owner]
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
    }

    pub(super) fn rebalance_queued_inode(&self, helper_id: usize) -> Result<bool> {
        if helper_id >= self.workers.len() {
            return_errno_with_message!(Errno::EINVAL, "invalid migration helper worker");
        }
        let Some((victim_id, victim)) = self
            .workers
            .iter()
            .enumerate()
            .filter(|(worker_id, _)| *worker_id != helper_id)
            .max_by_key(|(_, worker)| worker.queued_dirty_pages.load(Ordering::Acquire))
        else {
            return Ok(false);
        };
        if victim.queued_dirty_pages.load(Ordering::Acquire) == 0 {
            return Ok(false);
        }
        let Some(inode_map) = victim.ready_inodes.lock().pop_front() else {
            return Ok(false);
        };

        Self::record_migration_lock(&inode_map);
        let mut migration = inode_map.migration.lock();
        if migration.owner != Some(victim_id) {
            return Ok(false);
        }
        let queued_pages = inode_map.queued_dirty_pages.load(Ordering::Acquire);
        if inode_map.work_state.load(Ordering::Acquire) != INODE_WORK_QUEUED
            || queued_pages == 0
            || migration.in_flight_pages != 0
            || migration.completion_pending_pages != 0
            || migration.reclaim_pending_pages != 0
        {
            if inode_map.work_state.load(Ordering::Acquire) == INODE_WORK_QUEUED {
                victim.ready_inodes.lock().push_back(inode_map.clone());
            }
            return Ok(false);
        }

        migration.owner = Some(helper_id);
        migration.generation = migration.generation.wrapping_add(1);
        inode_map
            .migration_owner
            .store(helper_id, Ordering::Release);
        victim
            .queued_dirty_pages
            .fetch_sub(queued_pages, Ordering::AcqRel);
        self.workers[helper_id]
            .queued_dirty_pages
            .fetch_add(queued_pages, Ordering::AcqRel);
        victim.owned_inodes.fetch_sub(1, Ordering::AcqRel);
        self.workers[helper_id]
            .owned_inodes
            .fetch_add(1, Ordering::AcqRel);
        drop(migration);
        self.workers[helper_id]
            .ready_inodes
            .lock()
            .push_back(inode_map);
        Ok(true)
    }

    pub(super) fn ensure_dirty_ready(&self, inode_map: &Arc<InodeRedirectMap>) {
        let owner = {
            Self::record_migration_lock(inode_map);
            let migration = inode_map.migration.lock();
            let Some(owner) = migration.owner else {
                return;
            };
            owner
        };
        self.schedule_inode(inode_map, owner);
    }

    pub(super) fn owner(&self, inode_map: &InodeRedirectMap) -> Option<usize> {
        let owner = inode_map.migration_owner.load(Ordering::Acquire);
        (owner != Self::NO_OWNER).then_some(owner)
    }

    pub(super) fn pop_ready_inode(
        &self,
        worker_id: usize,
    ) -> Result<Option<Arc<InodeRedirectMap>>> {
        let worker = self
            .workers
            .get(worker_id)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let Some(inode_map) = worker.ready_inodes.lock().pop_front() else {
            if crate::fs::rnvpc::batch_metrics::enabled() {
                worker.ready_inode_empty.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(None);
        };
        worker.ready_inode_dequeues.fetch_add(1, Ordering::Relaxed);
        if inode_map.migration_owner.load(Ordering::Acquire) != worker_id
            || inode_map
                .work_state
                .compare_exchange(
                    INODE_WORK_QUEUED,
                    INODE_WORK_RUNNING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            if crate::fs::rnvpc::batch_metrics::enabled() {
                worker.ready_inode_invalid.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(None);
        }
        Ok(Some(inode_map))
    }

    pub(super) fn record_entry_state_conflict(&self, worker_id: usize) {
        if crate::fs::rnvpc::batch_metrics::enabled()
            && let Some(worker) = self.workers.get(worker_id)
        {
            worker.entry_state_conflicts.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn finish_inode_turn(
        &self,
        inode_map: &Arc<InodeRedirectMap>,
        worker_id: usize,
        has_dirty: bool,
    ) {
        if inode_map.migration_owner.load(Ordering::Acquire) != worker_id {
            return;
        }
        loop {
            let state = inode_map.work_state.load(Ordering::Acquire);
            let next = match state {
                INODE_WORK_RUNNING if !has_dirty => INODE_WORK_IDLE,
                INODE_WORK_RUNNING | INODE_WORK_RERUN => INODE_WORK_QUEUED,
                INODE_WORK_IDLE | INODE_WORK_QUEUED => return,
                _ => unreachable!("invalid inode migration work state"),
            };
            if inode_map
                .work_state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if next == INODE_WORK_QUEUED {
                self.workers[worker_id]
                    .ready_inodes
                    .lock()
                    .push_back(inode_map.clone());
            }
            return;
        }
    }

    pub(super) fn claim_pages(
        &self,
        inode_map: &InodeRedirectMap,
        worker_id: usize,
        pages: usize,
    ) -> Result<bool> {
        Self::record_migration_lock(inode_map);
        let mut migration = inode_map.migration.lock();
        if migration.owner != Some(worker_id) {
            return Ok(false);
        }
        if inode_map
            .queued_dirty_pages
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_sub(pages)
            })
            .is_err()
        {
            return_errno_with_message!(Errno::ESTALE, "migration dirty page accounting changed");
        }
        migration.in_flight_pages = migration.in_flight_pages.saturating_add(pages);
        self.workers[worker_id]
            .queued_dirty_pages
            .fetch_sub(pages, Ordering::AcqRel);
        self.workers[worker_id]
            .in_flight_pages
            .fetch_add(pages, Ordering::AcqRel);
        Ok(true)
    }

    pub(super) fn return_claim(
        &self,
        inode_map: &Arc<InodeRedirectMap>,
        entry: &Arc<DirtyNvExtentEntry>,
        worker_id: usize,
        pages: usize,
    ) {
        {
            Self::record_migration_lock(inode_map);
            let mut migration = inode_map.migration.lock();
            if migration.owner != Some(worker_id) || migration.in_flight_pages < pages {
                return;
            }
            migration.in_flight_pages -= pages;
            inode_map
                .queued_dirty_pages
                .fetch_add(pages, Ordering::AcqRel);
        }
        self.workers[worker_id]
            .in_flight_pages
            .fetch_sub(pages, Ordering::AcqRel);
        self.workers[worker_id]
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
        inode_map.enqueue_dirty_entry(entry);
        self.schedule_inode(inode_map, worker_id);
    }

    pub(super) fn complete_claim(
        &self,
        inode_map: &InodeRedirectMap,
        worker_id: usize,
        pages: usize,
    ) -> Result<()> {
        Self::record_migration_lock(inode_map);
        let mut migration = inode_map.migration.lock();
        if migration.owner != Some(worker_id) || migration.in_flight_pages < pages {
            return_errno_with_message!(Errno::ESTALE, "migration completion owner changed");
        }
        migration.in_flight_pages -= pages;
        migration.completion_pending_pages =
            migration.completion_pending_pages.saturating_add(pages);
        self.workers[worker_id]
            .in_flight_pages
            .fetch_sub(pages, Ordering::AcqRel);
        self.workers[worker_id]
            .completion_pending_pages
            .fetch_add(pages, Ordering::AcqRel);
        Ok(())
    }

    pub(super) fn remove_active_entry(
        &self,
        inode_map: &InodeRedirectMap,
        pages: usize,
    ) -> Result<()> {
        self.remove_active_entries(inode_map, 1, pages)
    }

    pub(super) fn remove_active_entries(
        &self,
        inode_map: &InodeRedirectMap,
        entry_count: usize,
        pages: usize,
    ) -> Result<()> {
        if entry_count == 0 || pages == 0 {
            return_errno!(Errno::EINVAL);
        }
        Self::record_migration_lock(inode_map);
        let mut migration = inode_map.migration.lock();
        let owner = migration.owner.ok_or_else(|| Error::new(Errno::ESTALE))?;
        if migration.active_entries < entry_count || migration.completion_pending_pages < pages {
            return_errno_with_message!(Errno::ESTALE, "migration entry accounting changed");
        }
        migration.active_entries -= entry_count;
        migration.completion_pending_pages -= pages;
        self.workers[owner]
            .completion_pending_pages
            .fetch_sub(pages, Ordering::AcqRel);
        if migration.active_entries == 0 {
            debug_assert_eq!(inode_map.queued_dirty_pages.load(Ordering::Acquire), 0);
            debug_assert_eq!(migration.in_flight_pages, 0);
            debug_assert_eq!(migration.completion_pending_pages, 0);
            debug_assert_eq!(migration.reclaim_pending_pages, 0);
            migration.owner = None;
            inode_map
                .migration_owner
                .store(Self::NO_OWNER, Ordering::Release);
            inode_map
                .work_state
                .store(INODE_WORK_IDLE, Ordering::Release);
            self.workers[owner]
                .owned_inodes
                .fetch_sub(1, Ordering::AcqRel);
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, worker_id: usize) -> Option<MigrationWorkerLoadSnapshot> {
        self.workers
            .get(worker_id)
            .map(MigrationWorkerState::snapshot)
    }
}
