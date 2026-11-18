// SPDX-License-Identifier: MPL-2.0

//! Each slot owns one lock protecting its runtime record.

use core::ops::{Index, IndexMut, Range};
#[cfg(ktest)]
use core::sync::atomic::{AtomicUsize, Ordering};

use ostd::sync::{PreemptDisabled, WaitQueue};

use super::ObjectRecord;
#[cfg(ktest)]
use super::ReverseExtentRef;
use crate::{
    fs::rnvpc::{redirect_index::DirtyNvExtentEntry, slot_allocator::allocation_domain_ranges},
    prelude::*,
};

pub(super) struct ObjectTable {
    slots: Vec<SlotEntry>,
    migration_ranges: SpinLock<MigrationRangeState>,
    data_range_domains: Box<[DataRangeDomain]>,
    #[cfg(ktest)]
    data_range_lock_acquisitions: AtomicUsize,
}

struct DataRangeOwner {
    end: usize,
    owner: Weak<DirtyNvExtentEntry>,
}

struct DataRangeDomain {
    start: usize,
    end: usize,
    owners: SpinLock<BTreeMap<usize, DataRangeOwner>>,
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
        let mut data_range_domains = Vec::new();
        for range in allocation_domain_ranges(pages)? {
            data_range_domains.push(DataRangeDomain {
                start: range.start,
                end: range.end,
                owners: SpinLock::new(BTreeMap::new()),
            });
        }
        Ok(Self {
            slots,
            migration_ranges: SpinLock::new(MigrationRangeState::default()),
            data_range_domains: data_range_domains.into_boxed_slice(),
            #[cfg(ktest)]
            data_range_lock_acquisitions: AtomicUsize::new(0),
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
        let mut ranges = self.lock_data_ranges(slots.clone())?;
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
        Ok(self
            .remove_data_range_owners(core::slice::from_ref(entry))?
            .pop()
            .unwrap())
    }

    pub(super) fn remove_data_range_owners(
        &self,
        entries: &[Arc<DirtyNvExtentEntry>],
    ) -> Result<Vec<Range<usize>>> {
        let mut pending = Vec::with_capacity(entries.len());
        for (position, entry) in entries.iter().enumerate() {
            let start = usize::try_from(entry.nv_slot_range().start)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            let range = start..start + usize::from(entry.page_count());
            let domain_index = self
                .data_range_domains
                .iter()
                .position(|domain| range.start >= domain.start && range.end <= domain.end)
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            pending.push((domain_index, position, start, entry));
        }
        pending.sort_unstable_by_key(|(domain, _, start, _)| (*domain, *start));
        let mut removed = (0..entries.len()).map(|_| None).collect::<Vec<_>>();
        let mut offset = 0;
        while offset < pending.len() {
            let domain_index = pending[offset].0;
            let end = pending[offset..]
                .iter()
                .position(|(domain, _, _, _)| *domain != domain_index)
                .map_or(pending.len(), |next| offset + next);
            #[cfg(ktest)]
            self.data_range_lock_acquisitions
                .fetch_add(1, Ordering::Relaxed);
            let mut ranges = self.data_range_domains[domain_index].owners.lock();
            for (_, _, start, entry) in &pending[offset..end] {
                let owner = ranges.get(start).ok_or_else(|| Error::new(Errno::ESTALE))?;
                if !owner
                    .owner
                    .upgrade()
                    .is_some_and(|candidate| Arc::ptr_eq(&candidate, entry))
                {
                    return_errno_with_message!(Errno::ESTALE, "NV data range owner changed");
                }
            }
            for (_, position, start, _) in &pending[offset..end] {
                let owner = ranges.remove(start).unwrap();
                removed[*position] = Some(*start..owner.end);
            }
            offset = end;
        }
        Ok(removed.into_iter().map(Option::unwrap).collect())
    }

    pub(super) fn extend_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
        expected_pages: u16,
        committed_pages: u16,
    ) -> Result<()> {
        let start = usize::try_from(entry.nv_slot_range().start)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let expected_end = start
            .checked_add(usize::from(expected_pages))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let committed_end = start
            .checked_add(usize::from(committed_pages))
            .filter(|end| *end <= self.slots.len())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let mut ranges = self.lock_data_ranges(start..committed_end)?;
        let owner = ranges
            .get(&start)
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if owner.end != expected_end
            || !owner
                .owner
                .upgrade()
                .is_some_and(|candidate| Arc::ptr_eq(&candidate, entry))
            || ranges
                .range((
                    core::ops::Bound::Excluded(start),
                    core::ops::Bound::Unbounded,
                ))
                .next()
                .is_some_and(|(next, _)| *next < committed_end)
        {
            return_errno_with_message!(Errno::ESTALE, "NV data range cannot be extended");
        }
        ranges.get_mut(&start).unwrap().end = committed_end;
        Ok(())
    }

    pub(super) fn shrink_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
        committed_pages: u16,
        expected_pages: u16,
    ) {
        let Ok(start) = usize::try_from(entry.nv_slot_range().start) else {
            return;
        };
        let Some(committed_end) = start.checked_add(usize::from(committed_pages)) else {
            return;
        };
        let Some(expected_end) = start.checked_add(usize::from(expected_pages)) else {
            return;
        };
        let Ok(mut ranges) = self.lock_data_ranges(start..committed_end) else {
            return;
        };
        if let Some(owner) = ranges.get_mut(&start)
            && owner.end == committed_end
            && owner
                .owner
                .upgrade()
                .is_some_and(|candidate| Arc::ptr_eq(&candidate, entry))
        {
            owner.end = expected_end;
        }
    }

    pub(super) fn data_range_owner(&self, entry: &Arc<DirtyNvExtentEntry>) -> Result<Range<usize>> {
        let start = usize::try_from(entry.nv_slot_range().start)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let ranges = self.lock_data_ranges(start..start + usize::from(entry.page_count()))?;
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

    pub(super) fn claim_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
    ) -> Result<Range<usize>> {
        self.data_range_owner(entry)
    }

    fn data_range_domain(&self, range: Range<usize>) -> Result<&DataRangeDomain> {
        if range.start >= range.end || range.end > self.slots.len() {
            return_errno!(Errno::EINVAL);
        }
        let domain = self
            .data_range_domains
            .iter()
            .find(|domain| range.start >= domain.start && range.start < domain.end)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if range.end > domain.end {
            return_errno_with_message!(Errno::EINVAL, "NV data range crosses allocation domains");
        }
        Ok(domain)
    }

    fn lock_data_ranges(
        &self,
        range: Range<usize>,
    ) -> Result<SpinLockGuard<'_, BTreeMap<usize, DataRangeOwner>, PreemptDisabled>> {
        let domain = self.data_range_domain(range)?;
        #[cfg(ktest)]
        self.data_range_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        Ok(domain.owners.lock())
    }

    #[cfg(ktest)]
    fn data_range_domain_count_for_test(&self) -> usize {
        self.data_range_domains.len()
    }

    #[cfg(ktest)]
    fn lock_data_range_domain_for_test(
        &self,
        slot: usize,
    ) -> Result<SpinLockGuard<'_, BTreeMap<usize, DataRangeOwner>, PreemptDisabled>> {
        self.lock_data_ranges(slot..slot + 1)
    }

    #[cfg(ktest)]
    fn try_lock_data_range_domain_for_test(&self, slot: usize) -> bool {
        self.data_range_domain(slot..slot + 1)
            .ok()
            .and_then(|domain| domain.owners.try_lock())
            .is_some()
    }

    #[cfg(ktest)]
    fn validate_data_range_domain_for_test(&self, range: Range<usize>) -> Result<()> {
        self.data_range_domain(range).map(|_| ())
    }

    #[cfg(ktest)]
    pub(super) fn reset_data_range_lock_acquisitions(&self) {
        self.data_range_lock_acquisitions
            .store(0, Ordering::Release);
    }

    #[cfg(ktest)]
    pub(super) fn data_range_lock_acquisitions(&self) -> usize {
        self.data_range_lock_acquisitions.load(Ordering::Acquire)
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

    #[ostd::prelude::ktest]
    fn data_range_owner_locks_are_partitioned_by_allocation_domain() {
        let table = ObjectTable::new(2_048).unwrap();
        assert!(table.data_range_domain_count_for_test() >= 2);

        let first = table.lock_data_range_domain_for_test(0).unwrap();
        assert!(table.try_lock_data_range_domain_for_test(1_024));
        drop(first);

        assert_eq!(
            table
                .validate_data_range_domain_for_test(1_023..1_025)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
    }
}
