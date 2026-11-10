// SPDX-License-Identifier: MPL-2.0

//! Volatile inode ownership for rNVPCe migration workers.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::InodeRedirectMap;
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MigrationWorkerLoadSnapshot {
    pub(crate) queued_dirty_pages: usize,
    pub(crate) in_flight_pages: usize,
    pub(crate) completion_pending_pages: usize,
    pub(crate) owned_inodes: usize,
    pub(crate) ready_inode_dequeues: usize,
}

#[derive(Default)]
pub(super) struct InodeMigrationState {
    pub(super) owner: Option<usize>,
    pub(super) generation: u64,
    pub(super) active_entries: usize,
    pub(super) queued_dirty_pages: usize,
    pub(super) in_flight_pages: usize,
    pub(super) completion_pending_pages: usize,
    pub(super) reclaim_pending_pages: usize,
    pub(super) ready_queued: bool,
}

struct MigrationWorkerState {
    ready_inodes: SpinLock<VecDeque<Arc<InodeRedirectMap>>>,
    queued_dirty_pages: AtomicUsize,
    in_flight_pages: AtomicUsize,
    completion_pending_pages: AtomicUsize,
    owned_inodes: AtomicUsize,
    ready_inode_dequeues: AtomicUsize,
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
        }
    }
}

pub(super) struct MigrationWorkerSet {
    workers: Box<[MigrationWorkerState]>,
    tie_break: AtomicUsize,
}

impl MigrationWorkerSet {
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

    pub(super) fn publish_dirty(self: &Arc<Self>, inode_map: &Arc<InodeRedirectMap>, pages: usize) {
        let (owner, enqueue) = {
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
                    owner
                }
            };
            migration.active_entries = migration.active_entries.saturating_add(1);
            migration.queued_dirty_pages = migration.queued_dirty_pages.saturating_add(pages);
            let enqueue = !migration.ready_queued;
            migration.ready_queued = true;
            (owner, enqueue)
        };
        self.workers[owner]
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
        if enqueue {
            self.workers[owner]
                .ready_inodes
                .lock()
                .push_back(inode_map.clone());
        }
    }

    pub(super) fn extend_dirty(&self, inode_map: &Arc<InodeRedirectMap>, pages: usize) {
        let (owner, enqueue) = {
            let mut migration = inode_map.migration.lock();
            let owner = migration
                .owner
                .expect("a registered dirty entry has a migration owner");
            migration.queued_dirty_pages = migration.queued_dirty_pages.saturating_add(pages);
            let enqueue = !migration.ready_queued;
            migration.ready_queued = true;
            (owner, enqueue)
        };
        self.workers[owner]
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
        if enqueue {
            self.workers[owner]
                .ready_inodes
                .lock()
                .push_back(inode_map.clone());
        }
    }

    pub(super) fn ensure_dirty_ready(&self, inode_map: &Arc<InodeRedirectMap>) {
        let (owner, enqueue) = {
            let mut migration = inode_map.migration.lock();
            let Some(owner) = migration.owner else {
                return;
            };
            let enqueue = !migration.ready_queued;
            migration.ready_queued = true;
            (owner, enqueue)
        };
        if enqueue {
            self.workers[owner]
                .ready_inodes
                .lock()
                .push_back(inode_map.clone());
        }
    }

    pub(super) fn owner(&self, inode_map: &InodeRedirectMap) -> Option<usize> {
        inode_map.migration.lock().owner
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
            return Ok(None);
        };
        worker.ready_inode_dequeues.fetch_add(1, Ordering::Relaxed);
        let mut migration = inode_map.migration.lock();
        if migration.owner == Some(worker_id) {
            migration.ready_queued = false;
            Ok(Some(inode_map.clone()))
        } else {
            Ok(None)
        }
    }

    pub(super) fn requeue_if_dirty(
        &self,
        inode_map: &Arc<InodeRedirectMap>,
        worker_id: usize,
        has_dirty: bool,
    ) {
        if !has_dirty {
            return;
        }
        let enqueue = {
            let mut migration = inode_map.migration.lock();
            if migration.owner != Some(worker_id) || migration.ready_queued {
                false
            } else {
                migration.ready_queued = true;
                true
            }
        };
        if enqueue {
            self.workers[worker_id]
                .ready_inodes
                .lock()
                .push_back(inode_map.clone());
        }
    }

    pub(super) fn claim_pages(
        &self,
        inode_map: &InodeRedirectMap,
        worker_id: usize,
        pages: usize,
    ) -> Result<()> {
        let mut migration = inode_map.migration.lock();
        if migration.owner != Some(worker_id) || migration.queued_dirty_pages < pages {
            return_errno_with_message!(Errno::ESTALE, "migration inode owner changed");
        }
        migration.queued_dirty_pages -= pages;
        migration.in_flight_pages = migration.in_flight_pages.saturating_add(pages);
        self.workers[worker_id]
            .queued_dirty_pages
            .fetch_sub(pages, Ordering::AcqRel);
        self.workers[worker_id]
            .in_flight_pages
            .fetch_add(pages, Ordering::AcqRel);
        Ok(())
    }

    pub(super) fn return_claim(
        &self,
        inode_map: &Arc<InodeRedirectMap>,
        worker_id: usize,
        pages: usize,
    ) {
        let enqueue = {
            let mut migration = inode_map.migration.lock();
            if migration.owner != Some(worker_id) || migration.in_flight_pages < pages {
                return;
            }
            migration.in_flight_pages -= pages;
            migration.queued_dirty_pages = migration.queued_dirty_pages.saturating_add(pages);
            let enqueue = !migration.ready_queued;
            migration.ready_queued = true;
            enqueue
        };
        self.workers[worker_id]
            .in_flight_pages
            .fetch_sub(pages, Ordering::AcqRel);
        self.workers[worker_id]
            .queued_dirty_pages
            .fetch_add(pages, Ordering::AcqRel);
        if enqueue {
            self.workers[worker_id]
                .ready_inodes
                .lock()
                .push_back(inode_map.clone());
        }
    }

    pub(super) fn complete_claim(
        &self,
        inode_map: &InodeRedirectMap,
        worker_id: usize,
        pages: usize,
    ) -> Result<()> {
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
        let mut migration = inode_map.migration.lock();
        let owner = migration.owner.ok_or_else(|| Error::new(Errno::ESTALE))?;
        if migration.active_entries == 0 || migration.completion_pending_pages < pages {
            return_errno_with_message!(Errno::ESTALE, "migration entry accounting changed");
        }
        migration.active_entries -= 1;
        migration.completion_pending_pages -= pages;
        self.workers[owner]
            .completion_pending_pages
            .fetch_sub(pages, Ordering::AcqRel);
        if migration.active_entries == 0 {
            debug_assert_eq!(migration.queued_dirty_pages, 0);
            debug_assert_eq!(migration.in_flight_pages, 0);
            debug_assert_eq!(migration.completion_pending_pages, 0);
            debug_assert_eq!(migration.reclaim_pending_pages, 0);
            migration.owner = None;
            migration.ready_queued = false;
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
