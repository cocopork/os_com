// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::sync::LocalIrqDisabled;

use crate::prelude::*;

struct ReadyTicket<T> {
    value: T,
    units: usize,
}

pub(in crate::fs::fs_impls::ext4) struct ReclaimReadyQueue<T> {
    queue: SpinLock<VecDeque<ReadyTicket<T>>, LocalIrqDisabled>,
    ready_units: AtomicUsize,
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
        self.queue.lock().push_back(ReadyTicket { value, units });
        self.ready_units.fetch_add(units, Ordering::Release);
    }

    pub(super) fn claim(&self) -> Option<(T, usize)> {
        let ticket = self.queue.lock().pop_front()?;
        self.ready_units.fetch_sub(ticket.units, Ordering::AcqRel);
        Some((ticket.value, ticket.units))
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

    use super::ReclaimReadyQueue;

    #[ktest]
    fn reclaim_ready_ticket_has_one_owner() {
        let queue = ReclaimReadyQueue::new();
        queue.publish(11, 32);

        assert_eq!(queue.claim(), Some((11, 32)));
        assert_eq!(queue.claim(), None);
        assert_eq!(queue.ready_units(), 0);
    }

    #[ktest]
    fn reclaim_ready_queue_accounts_each_ticket_exactly_once() {
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
