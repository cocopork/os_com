// SPDX-License-Identifier: MPL-2.0

//! Each slot owns one lock protecting its runtime record.

use core::ops::{Index, IndexMut};

use ostd::sync::{PreemptDisabled, WaitQueue};

use super::ObjectRecord;
#[cfg(ktest)]
use super::ReverseExtentRef;
use crate::prelude::*;

pub(super) struct ObjectTable {
    slots: Vec<SlotEntry>,
}

struct SlotEntry {
    state: SpinLock<ObjectSlot>,
    metadata_mutation_waiters: WaitQueue,
}

pub(super) struct ObjectSlot {
    pub(super) records: SlotRecords,
    #[cfg(ktest)]
    pub(super) reverse: Option<ReverseExtentRef>,
    pub(super) metadata_mutation: Option<u64>,
}

pub(super) struct SlotRecords {
    slot: usize,
    record: ObjectRecord,
}

impl ObjectTable {
    pub(super) fn new(pages: usize) -> Result<Self> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(pages)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for slot in 0..pages {
            slots.push(SlotEntry {
                state: SpinLock::new(ObjectSlot {
                    records: SlotRecords {
                        slot,
                        record: ObjectRecord::free(),
                    },
                    #[cfg(ktest)]
                    reverse: None,
                    metadata_mutation: None,
                }),
                metadata_mutation_waiters: WaitQueue::new(),
            });
        }
        Ok(Self { slots })
    }

    // Lock order: pool control state, allocation state, then one slot. Never
    // acquire either outer lock or another slot while holding a slot. Persistence and
    // sleeping happen after dropping both locks; pins/leases keep objects alive.
    pub(super) fn lock(&self, slot: usize) -> SpinLockGuard<'_, ObjectSlot, PreemptDisabled> {
        self.slots[slot].state.lock()
    }

    pub(super) fn lock_checked(
        &self,
        slot: usize,
    ) -> Result<SpinLockGuard<'_, ObjectSlot, PreemptDisabled>> {
        self.slots
            .get(slot)
            .map(|entry| entry.state.lock())
            .ok_or_else(|| Error::new(Errno::EINVAL))
    }

    pub(super) fn get_mut(&mut self, slot: usize) -> &mut ObjectRecord {
        &mut self.slots[slot].state.get_mut().records[slot]
    }

    pub(super) fn get(&self, slot: usize) -> Option<ObjectRecord> {
        self.slots
            .get(slot)
            .map(|entry| entry.state.lock().records.record)
    }

    #[cfg(ktest)]
    pub(super) fn try_claim_metadata_mutation(
        &self,
        slot: usize,
        allocation_seq: u64,
    ) -> Result<bool> {
        let mut state = self.lock_checked(slot)?;
        if state.metadata_mutation.is_some() {
            return Ok(false);
        }
        state.metadata_mutation = Some(allocation_seq);
        Ok(true)
    }

    pub(super) fn metadata_mutation_matches(
        &self,
        slot: usize,
        allocation_seq: u64,
    ) -> Result<bool> {
        Ok(self.lock_checked(slot)?.metadata_mutation == Some(allocation_seq))
    }

    /// Sleeps without holding the slot spin lock until a semantic mutation
    /// owner releases the slot. The caller must revalidate after waking.
    pub(super) fn wait_for_metadata_mutation_release(&self, slot: usize) -> Result<()> {
        let entry = self
            .slots
            .get(slot)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        entry
            .metadata_mutation_waiters
            .wait_until(|| entry.state.lock().metadata_mutation.is_none().then_some(()));
        Ok(())
    }

    pub(super) fn release_metadata_mutation(&self, slot: usize, allocation_seq: u64) -> Result<()> {
        let mut state = self.lock_checked(slot)?;
        if state.metadata_mutation == Some(allocation_seq) {
            state.metadata_mutation = None;
            drop(state);
            self.slots[slot].metadata_mutation_waiters.wake_all();
        }
        Ok(())
    }

    /// Collects management/recovery results without nesting slot locks.
    pub(super) fn collect<T>(
        &self,
        mut visit: impl FnMut(usize, &ObjectRecord) -> Option<T>,
    ) -> Vec<T> {
        let mut result = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(value) = visit(index, &slot.state.lock().records.record) {
                result.push(value);
            }
        }
        result
    }
}

impl SlotRecords {
    pub(super) fn get(&self, slot: usize) -> Option<&ObjectRecord> {
        (slot == self.slot).then_some(&self.record)
    }

    pub(super) fn get_mut(&mut self, slot: usize) -> Option<&mut ObjectRecord> {
        (slot == self.slot).then_some(&mut self.record)
    }
}

impl Index<usize> for SlotRecords {
    type Output = ObjectRecord;

    fn index(&self, slot: usize) -> &Self::Output {
        self.get(slot).expect("valid NVPool slot")
    }
}

impl IndexMut<usize> for SlotRecords {
    fn index_mut(&mut self, slot: usize) -> &mut Self::Output {
        self.get_mut(slot).expect("valid NVPool slot")
    }
}

#[cfg(ktest)]
mod tests {
    use super::*;

    #[ostd::prelude::ktest]
    fn rnvpce_object_slots_isolate_former_shard_aliases() {
        let table = ObjectTable::new(65).unwrap();
        let first = table.lock(0);
        assert!(table.slots[0].state.try_lock().is_none());
        assert!(table.slots[1].state.try_lock().is_some());
        assert!(table.slots[64].state.try_lock().is_some());
        assert!(first.records.get(64).is_none());
        drop(first);
        assert!(table.slots[0].state.try_lock().is_some());
    }

    #[ostd::prelude::ktest]
    fn metadata_mutation_claim_is_exclusive_and_generation_scoped() {
        let table = ObjectTable::new(1).unwrap();

        assert!(table.try_claim_metadata_mutation(0, 7).unwrap());
        assert!(!table.try_claim_metadata_mutation(0, 7).unwrap());
        table.release_metadata_mutation(0, 8).unwrap();
        assert!(!table.try_claim_metadata_mutation(0, 7).unwrap());
        table.release_metadata_mutation(0, 7).unwrap();
        assert!(table.try_claim_metadata_mutation(0, 7).unwrap());
    }
}
