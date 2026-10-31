// SPDX-License-Identifier: MPL-2.0

//! Each slot owns one lock protecting its runtime record.

use core::ops::{Index, IndexMut, Range};

use ostd::sync::{PreemptDisabled, WaitQueue};

use super::ObjectRecord;
#[cfg(ktest)]
use super::ReverseExtentRef;
use crate::{fs::rnvpc::redirect_index::DirtyNvExtentEntry, prelude::*};

pub(super) struct ObjectTable {
    slots: Vec<SlotEntry>,
    migration_ranges: SpinLock<MigrationRangeState>,
    data_ranges: SpinLock<BTreeMap<usize, DataRangeOwner>>,
}

struct DataRangeOwner {
    end: usize,
    owner: Weak<DirtyNvExtentEntry>,
}

#[derive(Default)]
struct MigrationRangeState {
    active: BTreeMap<usize, (usize, u64)>,
    next_generation: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct NvSlotRangeLease {
    base: usize,
    end: usize,
    generation: u64,
}

impl NvSlotRangeLease {
    pub(super) fn base(&self) -> usize {
        self.base
    }

    pub(super) fn len(&self) -> usize {
        self.end - self.base
    }
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
        Ok(Self {
            slots,
            migration_ranges: SpinLock::new(MigrationRangeState::default()),
            data_ranges: SpinLock::new(BTreeMap::new()),
        })
    }

    pub(super) fn install_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
        slots: Range<usize>,
    ) -> Result<()> {
        if slots.start >= slots.end
            || slots.end > self.slots.len()
            || slots.end - slots.start != usize::from(entry.page_count())
            || u64::try_from(slots.start).ok() != Some(entry.nv_slot_range().start)
            || u64::try_from(slots.end).ok() != Some(entry.nv_slot_range().end)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid NV data range owner");
        }
        let mut ranges = self.data_ranges.lock();
        if ranges
            .range(..slots.end)
            .next_back()
            .is_some_and(|(_, owner)| owner.end > slots.start)
        {
            return_errno_with_message!(Errno::EBUSY, "NV data range owner overlaps");
        }
        ranges.insert(
            slots.start,
            DataRangeOwner {
                end: slots.end,
                owner: Arc::downgrade(entry),
            },
        );
        Ok(())
    }

    pub(super) fn remove_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
    ) -> Result<Range<usize>> {
        let start = usize::try_from(entry.nv_slot_range().start)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let mut ranges = self.data_ranges.lock();
        let owner = ranges
            .get(&start)
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if !owner
            .owner
            .upgrade()
            .is_some_and(|candidate| Arc::ptr_eq(&candidate, entry))
        {
            return_errno_with_message!(Errno::ESTALE, "NV data range owner changed");
        }
        let end = owner.end;
        ranges.remove(&start);
        Ok(start..end)
    }

    pub(super) fn data_range_owner(&self, entry: &Arc<DirtyNvExtentEntry>) -> Result<Range<usize>> {
        let start = usize::try_from(entry.nv_slot_range().start)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let ranges = self.data_ranges.lock();
        let owner = ranges
            .get(&start)
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if !owner
            .owner
            .upgrade()
            .is_some_and(|candidate| Arc::ptr_eq(&candidate, entry))
        {
            return_errno_with_message!(Errno::ESTALE, "NV data range owner changed");
        }
        Ok(start..owner.end)
    }

    pub(super) fn claim_migration_range(
        &self,
        base: usize,
        len: usize,
    ) -> Result<NvSlotRangeLease> {
        if len == 0 || len > 128 {
            return_errno!(Errno::EINVAL);
        }
        let end = base
            .checked_add(len)
            .filter(|end| *end <= self.slots.len())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let mut state = self.migration_ranges.lock();
        if state
            .active
            .range(..end)
            .next_back()
            .is_some_and(|(_, (active_end, _))| *active_end > base)
        {
            return_errno_with_message!(Errno::EBUSY, "NVPage range is already migrating");
        }
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let generation = state.next_generation;
        state.active.insert(base, (end, generation));
        Ok(NvSlotRangeLease {
            base,
            end,
            generation,
        })
    }

    pub(super) fn release_migration_range(&self, lease: NvSlotRangeLease) -> Result<()> {
        let mut state = self.migration_ranges.lock();
        if state.active.get(&lease.base) != Some(&(lease.end, lease.generation)) {
            return_errno!(Errno::ESTALE);
        }
        state.active.remove(&lease.base);
        Ok(())
    }

    pub(super) fn migration_range_contains(&self, slot: usize) -> bool {
        self.migration_ranges
            .lock()
            .active
            .range(..=slot)
            .next_back()
            .is_some_and(|(_, (end, _))| slot < *end)
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

    #[ostd::prelude::ktest]
    fn nv_slot_range_lease_excludes_only_overlapping_slots() {
        let table = ObjectTable::new(512).unwrap();
        let first = table.claim_migration_range(100, 128).unwrap();
        assert_eq!(
            table.claim_migration_range(127, 2).unwrap_err().error(),
            Errno::EBUSY
        );
        let disjoint = table.claim_migration_range(228, 128).unwrap();
        assert!(table.migration_range_contains(100));
        assert!(!table.migration_range_contains(99));
        table.release_migration_range(disjoint).unwrap();
        table.release_migration_range(first).unwrap();
        assert!(!table.migration_range_contains(100));
    }
}
