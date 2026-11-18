// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::sync::LocalIrqDisabled;

use crate::prelude::*;

struct ReadyEntry<T> {
    value: T,
    units: usize,
}

pub(in crate::fs::fs_impls::ext4) struct ReclaimReadyQueue<T> {
    queue: SpinLock<VecDeque<ReadyEntry<T>>, LocalIrqDisabled>,
    ready_units: AtomicUsize,
}

pub(in crate::fs::fs_impls::ext4) struct ReclaimQueueSet<T> {
    queues: Box<[ReclaimReadyQueue<T>]>,
    total_ready_units: AtomicUsize,
}

impl<T> ReclaimQueueSet<T> {
    pub(super) fn new(worker_count: usize) -> Result<Self> {
        if worker_count == 0 {
            return_errno_with_message!(Errno::EINVAL, "reclaim queue set requires one worker");
        }
        let mut queues = Vec::new();
        queues
            .try_reserve_exact(worker_count)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        queues.resize_with(worker_count, ReclaimReadyQueue::new);
        Ok(Self {
            queues: queues.into_boxed_slice(),
            total_ready_units: AtomicUsize::new(0),
        })
    }

    pub(super) fn publish(&self, worker_id: usize, value: T, units: usize) -> Result<()> {
        let queue = self
            .queues
            .get(worker_id)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        queue.publish(value, units);
        self.total_ready_units.fetch_add(units, Ordering::Release);
        Ok(())
    }

    pub(super) fn claim(&self, worker_id: usize) -> Result<Option<(T, usize)>> {
        let queue = self
            .queues
            .get(worker_id)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let Some((value, units)) = queue.claim() else {
            return Ok(None);
        };
        self.total_ready_units.fetch_sub(units, Ordering::AcqRel);
        Ok(Some((value, units)))
    }

    pub(super) fn worker_ready_units(&self, worker_id: usize) -> Option<usize> {
        self.queues
            .get(worker_id)
            .map(ReclaimReadyQueue::ready_units)
    }

    pub(super) fn total_ready_units(&self) -> usize {
        self.total_ready_units.load(Ordering::Acquire)
    }

    pub(super) fn worker_count(&self) -> usize {
        self.queues.len()
    }
}

impl<T> ReclaimReadyQueue<T> {
    pub(super) const fn new() -> Self {
        Self {
            queue: SpinLock::new(VecDeque::new()),
            ready_units: AtomicUsize::new(0),
        }
    }

    pub(super) fn publish(&self, value: T, units: usize) {
        debug_assert!(units != 0);
        self.queue.lock().push_back(ReadyEntry { value, units });
        self.ready_units.fetch_add(units, Ordering::Release);
    }

    pub(super) fn claim(&self) -> Option<(T, usize)> {
        let entry = self.queue.lock().pop_front()?;
        self.ready_units.fetch_sub(entry.units, Ordering::AcqRel);
        Some((entry.value, entry.units))
    }

    pub(super) fn ready_units(&self) -> usize {
        self.ready_units.load(Ordering::Acquire)
    }
}

#[cfg(ktest)]
pub(super) fn assert_queue_contract() {
    let queue = ReclaimReadyQueue::new();
    queue.publish(1, 4);
    queue.publish(2, 32);
    assert_eq!(queue.ready_units(), 36);
    assert_eq!(queue.claim(), Some((1, 4)));
    assert_eq!(queue.claim(), Some((2, 32)));
    assert_eq!(queue.claim(), None);
    assert_eq!(queue.ready_units(), 0);
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::{ReclaimQueueSet, ReclaimReadyQueue};

    #[ktest]
    fn reclaim_queue_set_isolates_workers_and_accounts_total_units() {
        let queues = ReclaimQueueSet::new(4).unwrap();
        queues.publish(1, 11, 4).unwrap();
        queues.publish(3, 33, 32).unwrap();

        assert_eq!(queues.worker_ready_units(0), Some(0));
        assert_eq!(queues.worker_ready_units(1), Some(4));
        assert_eq!(queues.worker_ready_units(3), Some(32));
        assert_eq!(queues.total_ready_units(), 36);
        assert_eq!(queues.claim(0).unwrap(), None);
        assert_eq!(queues.claim(1).unwrap(), Some((11, 4)));
        assert_eq!(queues.total_ready_units(), 32);
        assert!(queues.publish(4, 44, 1).is_err());
        assert!(queues.claim(4).is_err());
        assert_eq!(queues.claim(3).unwrap(), Some((33, 32)));
        assert_eq!(queues.total_ready_units(), 0);
    }

    #[ktest]
    fn reclaim_ready_work_item_has_one_owner() {
        let queue = ReclaimReadyQueue::new();
        queue.publish(11, 32);

        assert_eq!(queue.claim(), Some((11, 32)));
        assert_eq!(queue.claim(), None);
        assert_eq!(queue.ready_units(), 0);
    }

    #[ktest]
    fn reclaim_ready_queue_accounts_each_work_item_exactly_once() {
        let queue = ReclaimReadyQueue::new();
        queue.publish(1, 4);
        queue.publish(2, 32);

        assert_eq!(queue.ready_units(), 36);
        assert_eq!(queue.claim(), Some((1, 4)));
        assert_eq!(queue.ready_units(), 32);
        assert_eq!(queue.claim(), Some((2, 32)));
        assert_eq!(queue.ready_units(), 0);
    }
}
