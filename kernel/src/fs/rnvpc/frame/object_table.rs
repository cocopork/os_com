// SPDX-License-Identifier: MPL-2.0

//! Each slot owns one lock protecting its record and reverse locator.

use core::ops::{Index, IndexMut};

use ostd::sync::PreemptDisabled;

use super::{ObjectRecord, ReverseExtentRef};
use crate::prelude::*;

pub(super) struct ObjectTable {
    slots: Vec<SpinLock<ObjectSlot>>,
}

pub(super) struct ObjectSlot {
    pub(super) records: SlotRecords,
    pub(super) reverse: Option<ReverseExtentRef>,
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
            slots.push(SpinLock::new(ObjectSlot {
                records: SlotRecords {
                    slot,
                    record: ObjectRecord::free(),
                },
                reverse: None,
            }));
        }
        Ok(Self { slots })
    }

    // Lock order: pool control state, allocation state, then one slot. Never
    // acquire either outer lock or another slot while holding a slot. Persistence and
    // sleeping happen after dropping both locks; pins/leases keep objects alive.
    pub(super) fn lock(&self, slot: usize) -> SpinLockGuard<'_, ObjectSlot, PreemptDisabled> {
        self.slots[slot].lock()
    }

    pub(super) fn lock_checked(
        &self,
        slot: usize,
    ) -> Result<SpinLockGuard<'_, ObjectSlot, PreemptDisabled>> {
        self.slots
            .get(slot)
            .map(|entry| entry.lock())
            .ok_or_else(|| Error::new(Errno::EINVAL))
    }

    pub(super) fn get_mut(&mut self, slot: usize) -> &mut ObjectRecord {
        &mut self.slots[slot].get_mut().records[slot]
    }

    pub(super) fn get(&self, slot: usize) -> Option<ObjectRecord> {
        self.slots
            .get(slot)
            .map(|entry| entry.lock().records.record)
    }

    /// Collects management/recovery results without nesting slot locks.
    pub(super) fn collect<T>(
        &self,
        mut visit: impl FnMut(usize, &ObjectRecord) -> Option<T>,
    ) -> Vec<T> {
        let mut result = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(value) = visit(index, &slot.lock().records.record) {
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
        assert!(table.slots[0].try_lock().is_none());
        assert!(table.slots[1].try_lock().is_some());
        assert!(table.slots[64].try_lock().is_some());
        assert!(first.records.get(64).is_none());
        drop(first);
        assert!(table.slots[0].try_lock().is_some());
    }
}
