// SPDX-License-Identifier: MPL-2.0

pub(super) mod free_space;
pub(super) mod metrics;

use super::prelude::*;

/// Capacity authorization, never held while searching a group or doing I/O.
pub(super) struct Capacity {
    state: Mutex<CapacityState>,
}

struct CapacityState {
    physical_free: u64,
    inflight: u64,
    rnvpce: u64,
    delayed: u64,
    next_owner: u64,
    owners: BTreeMap<u64, u64>,
}

pub(super) struct Reservation<'a> {
    capacity: &'a Capacity,
    count: u64,
    owner: Option<u64>,
}

/// Owns a persistent capacity promise; no physical blocks are selected.
pub(super) struct DelayedAccount {
    capacity: Arc<Capacity>,
    id: u64,
}

impl Capacity {
    pub(super) fn new(free: u64) -> Self {
        Self {
            state: Mutex::new(CapacityState {
                physical_free: free,
                inflight: 0,
                rnvpce: 0,
                delayed: 0,
                next_owner: 1,
                owners: BTreeMap::new(),
            }),
        }
    }
    pub(super) fn claim(
        &self,
        count: u32,
        floor: u64,
        normal: bool,
        mut pinned: impl FnMut() -> u64,
    ) -> Result<Reservation<'_>> {
        loop {
            let waiting = metrics::Span::new(&metrics::CAPACITY_WAIT);
            let mut state = self.state.lock();
            drop(waiting);
            let holding = metrics::Span::new(&metrics::CAPACITY_HOLD);
            let withheld = if normal {
                floor
                    .saturating_add(state.rnvpce)
                    .saturating_add(state.delayed)
            } else {
                state.delayed
            };
            let reusable = state
                .physical_free
                .saturating_sub(pinned())
                .saturating_sub(withheld);
            let available = reusable.saturating_sub(state.inflight);
            let granted = (count as u64).min(available);
            if granted != 0 {
                state.inflight += granted;
                return Ok(Reservation {
                    capacity: self,
                    count: granted,
                    owner: None,
                });
            }
            // Only in-progress allocations can resolve this wait. No journal
            // commit is required and no filesystem allocation lock is held.
            // Distinguish this from genuinely unavailable/pinned capacity,
            // whose ENOSPC must unwind the handle before a commit can run.
            if reusable != 0 && state.inflight != 0 {
                drop(state);
                drop(holding);
                crate::thread::Thread::yield_now();
                continue;
            }
            return_errno!(Errno::ENOSPC);
        }
    }

    pub(super) fn reserve_rnvpce(
        &self,
        count: u64,
        floor: u64,
        mut pinned: impl FnMut() -> u64,
    ) -> Result<()> {
        loop {
            let mut state = self.state.lock();
            let reusable = state
                .physical_free
                .saturating_sub(pinned())
                .saturating_sub(floor)
                .saturating_sub(state.delayed);
            let next = state
                .rnvpce
                .checked_add(count)
                .filter(|next| *next <= reusable)
                .ok_or_else(|| Error::new(Errno::ENOSPC))?;
            if next <= reusable.saturating_sub(state.inflight) {
                state.rnvpce = next;
                return Ok(());
            }
            // The promise fits after temporary claims finish. As for claim(),
            // no pin/commit wait is attempted under the caller's inode lock.
            drop(state);
            crate::thread::Thread::yield_now();
        }
    }

    pub(super) fn release_rnvpce(&self, count: u64) -> Result<()> {
        let mut state = self.state.lock();
        state.rnvpce = state
            .rnvpce
            .checked_sub(count)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        Ok(())
    }

    pub(super) fn credit(&self, count: u64) -> Result<()> {
        let mut state = self.state.lock();
        state.physical_free = state
            .physical_free
            .checked_add(count)
            .ok_or_else(|| Error::new(Errno::EIO))?;
        Ok(())
    }

    pub(super) fn reconcile_rnvpce(&self, count: u64) {
        self.state.lock().rnvpce = count;
    }

    /// Mount/replay only, before concurrent allocation begins.
    pub(super) fn reset(&self, free: u64) {
        let mut state = self.state.lock();
        assert_eq!(state.inflight, 0);
        state.physical_free = free;
    }
}
impl Capacity {
    pub(super) fn delayed_account(self: &Arc<Self>) -> DelayedAccount {
        let mut state = self.state.lock();
        let id = state.next_owner;
        state.next_owner = id.checked_add(1).expect("delayed owner exhausted");
        state.owners.insert(id, 0);
        DelayedAccount {
            capacity: self.clone(),
            id,
        }
    }

    pub(super) fn claim_delayed(&self, count: u32, owner: u64) -> Result<Reservation<'_>> {
        let mut state = self.state.lock();
        let promised = state
            .owners
            .get_mut(&owner)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let granted = (*promised).min(count as u64);
        if granted == 0 {
            return_errno!(Errno::ENOSPC);
        }
        *promised -= granted;
        state.delayed -= granted;
        state.inflight += granted;
        Ok(Reservation {
            capacity: self,
            count: granted,
            owner: Some(owner),
        })
    }
}

impl DelayedAccount {
    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn remaining(&self) -> u64 {
        *self
            .capacity
            .state
            .lock()
            .owners
            .get(&self.id)
            .expect("live delayed owner")
    }

    pub(super) fn reserve(&self, count: u64, floor: u64, pinned: u64) -> Result<()> {
        loop {
            let mut state = self.capacity.state.lock();
            let reusable = state
                .physical_free
                .saturating_sub(pinned)
                .saturating_sub(floor)
                .saturating_sub(state.rnvpce)
                .saturating_sub(state.delayed);
            if count > reusable {
                return_errno!(Errno::ENOSPC);
            }
            if count <= reusable.saturating_sub(state.inflight) {
                *state.owners.get_mut(&self.id).expect("live delayed owner") += count;
                state.delayed += count;
                return Ok(());
            }
            drop(state);
            crate::thread::Thread::yield_now();
        }
    }

    /// Releases excess promise after cancellation or successful materialization.
    pub(super) fn retain(&self, count: u64) -> Result<()> {
        let mut state = self.capacity.state.lock();
        let remaining = state.owners.get_mut(&self.id).expect("live delayed owner");
        if count > *remaining {
            return_errno!(Errno::EUCLEAN);
        }
        let released = *remaining - count;
        *remaining = count;
        state.delayed -= released;
        Ok(())
    }
}

impl Drop for DelayedAccount {
    fn drop(&mut self) {
        let mut state = self.capacity.state.lock();
        let remaining = state.owners.remove(&self.id).expect("live delayed owner");
        state.delayed -= remaining;
    }
}

impl Reservation<'_> {
    pub(super) fn count(&self) -> u32 {
        self.count as u32
    }

    pub(super) fn consume(mut self, count: u64) -> Result<()> {
        let mut state = self.capacity.state.lock();
        if count > self.count || count > state.physical_free {
            return_errno!(Errno::EIO);
        }
        state.physical_free -= count;
        state.inflight -= self.count;
        if let Some(owner) = self.owner {
            let unused = self.count - count;
            *state.owners.get_mut(&owner).expect("live delayed owner") += unused;
            state.delayed += unused;
        }
        self.count = 0;
        Ok(())
    }

    /// A partial mutation cannot be reused after abort, even if its exact
    /// physical state is uncertain. Conservatively consume the whole claim.
    pub(super) fn poison(self) {
        let count = self.count;
        let _ = self.consume(count);
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.count != 0 {
            let mut state = self.capacity.state.lock();
            state.inflight -= self.count;
            if let Some(owner) = self.owner {
                *state.owners.get_mut(&owner).expect("live delayed owner") += self.count;
                state.delayed += self.count;
            }
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn ext4_capacity_reservations() {
        crate::fs::fs_impls::ext4::test_utils::scenarios!(
            delayed_owners_isolate_and_refund_promises,
            rnvpce_reservation_waits_for_partial_or_dropped_claim,
            capacity_inflight_is_not_permanent_enospc,
            capacity_reservation_partial_drop_and_floor,
            capacity_rnvpce_and_pending_free_share_authority,
        );
    }

    fn delayed_owners_isolate_and_refund_promises() {
        let capacity = Arc::new(Capacity::new(100));
        let first = capacity.delayed_account();
        let second = capacity.delayed_account();
        first.reserve(60, 10, 5).unwrap();
        second.reserve(10, 10, 5).unwrap();
        assert!(second.reserve(16, 10, 5).is_err());
        assert_eq!(capacity.claim(100, 0, false, || 0).unwrap().count(), 30);
        let owned = capacity.claim_delayed(40, first.id()).unwrap();
        assert_eq!(first.remaining(), 20);
        assert_eq!(capacity.claim(100, 0, false, || 0).unwrap().count(), 30);
        assert_eq!(
            capacity.claim_delayed(100, second.id()).unwrap().count(),
            10
        );
        owned.consume(7).unwrap();
        assert_eq!(first.remaining(), 53);
        let retry = capacity.claim_delayed(20, first.id()).unwrap();
        drop(retry);
        assert_eq!(first.remaining(), 53);
        assert!(capacity.reserve_rnvpce(31, 0, || 0).is_err());
        first.retain(0).unwrap();
        drop(second);
        assert_eq!(capacity.claim(100, 0, false, || 0).unwrap().count(), 93);
    }

    fn rnvpce_reservation_waits_for_partial_or_dropped_claim() {
        crate::time::clocks::init_for_ktest();
        for consumed in [0, 1] {
            let capacity = Arc::new(Capacity::new(100));
            let reservation = capacity.claim(100, 0, false, || 0).unwrap();
            let outcome = Arc::new(core::sync::atomic::AtomicUsize::new(0));
            let worker_capacity = capacity.clone();
            let worker_outcome = outcome.clone();
            let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
                let result = worker_capacity.reserve_rnvpce(1, 0, || 0);
                worker_outcome.store(
                    if result.is_ok() { 1 } else { 2 },
                    core::sync::atomic::Ordering::Release,
                );
            })
            .spawn();
            for _ in 0..32 {
                crate::thread::Thread::yield_now();
            }
            if consumed == 0 {
                drop(reservation);
            } else {
                reservation.consume(consumed).unwrap();
            }
            worker.join();
            assert_eq!(outcome.load(core::sync::atomic::Ordering::Acquire), 1);
            capacity.release_rnvpce(1).unwrap();
        }
    }

    fn capacity_inflight_is_not_permanent_enospc() {
        crate::time::clocks::init_for_ktest();
        let capacity = Arc::new(Capacity::new(100));
        let reservation = capacity.claim(100, 0, false, || 0).unwrap();
        let outcome = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let worker_capacity = capacity.clone();
        let worker_outcome = outcome.clone();
        let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
            let result = worker_capacity.claim(10, 0, false, || 0);
            worker_outcome.store(
                if result.is_ok() { 1 } else { 2 },
                core::sync::atomic::Ordering::Release,
            );
        })
        .spawn();
        for _ in 0..32 {
            crate::thread::Thread::yield_now();
        }
        drop(reservation);
        worker.join();
        assert_eq!(
            outcome.load(core::sync::atomic::Ordering::Acquire),
            1,
            "another allocator's temporary reservation must not cause permanent ENOSPC"
        );
    }

    fn capacity_reservation_partial_drop_and_floor() {
        let capacity = Capacity::new(100);
        let first = capacity.claim(80, 10, true, || 0).unwrap();
        assert_eq!(capacity.claim(80, 10, true, || 0).unwrap().count(), 10);
        first.consume(40).unwrap();
        assert_eq!(capacity.claim(80, 10, true, || 0).unwrap().count(), 50);
        capacity.credit(40).unwrap();
        assert_eq!(capacity.claim(100, 10, true, || 0).unwrap().count(), 90);
        assert_eq!(capacity.state.lock().inflight, 0);
    }

    fn capacity_rnvpce_and_pending_free_share_authority() {
        let capacity = Capacity::new(100);
        capacity.reserve_rnvpce(60, 10, || 0).unwrap();
        let first = capacity.claim(40, 10, true, || 5).unwrap();
        assert_eq!(first.count(), 25);
        assert!(capacity.reserve_rnvpce(30, 10, || 5).is_err());
        drop(first);
        capacity.release_rnvpce(60).unwrap();
        let all = capacity.claim(100, 0, false, || 100);
        assert!(all.is_err());
        let all = capacity.claim(100, 0, false, || 0).unwrap();
        assert_eq!(all.count(), 100);
        assert!(capacity.reserve_rnvpce(101, 0, || 0).is_err());
        drop(all);
        assert!(capacity.release_rnvpce(1).is_err());
    }
}
