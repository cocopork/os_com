// SPDX-License-Identifier: MPL-2.0

//! Volatile exact-page redirect and contiguous-page-range indexes for rNVPCe.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::{descriptor::NvObjectState, index::PageKey, metadata::PersistentObjectRef};
use crate::prelude::*;

const REDIRECT_PARTITION_COUNT: usize = 64;
const REDIRECT_PARTITION_PAGES: u64 = 256;
pub(crate) const MAX_DIRTY_MIGRATION_EXTENT_PAGES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DescriptorHandle {
    pub(crate) reference: PersistentObjectRef,
    pub(crate) logical_version: u64,
    pub(crate) state: NvObjectState,
    pub(crate) ssd_target: u64,
    pub(crate) migration_target: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RedirectRange {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) logical_start: u64,
    pub(crate) pages: Vec<DescriptorHandle>,
    pub(crate) epoch: u64,
    partition: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirtyMigrationExtent {
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    pages: Vec<DescriptorHandle>,
    partition: usize,
}

impl DirtyMigrationExtent {
    pub(crate) fn try_new(
        partition: usize,
        entries: Vec<(PageKey, DescriptorHandle)>,
    ) -> Result<Self> {
        let Some((first_key, first_handle)) = entries.first().copied() else {
            return_errno_with_message!(Errno::EINVAL, "empty dirty migration extent");
        };
        if entries.len() > MAX_DIRTY_MIGRATION_EXTENT_PAGES {
            return_errno_with_message!(Errno::E2BIG, "dirty migration extent is too large");
        }
        if first_handle.state != NvObjectState::NvDirty
            || redirect_partition_index(first_key) != partition
        {
            return_errno_with_message!(Errno::EINVAL, "invalid dirty migration extent head");
        }
        for pair in entries.windows(2) {
            if !dirty_entries_are_contiguous(pair[0], pair[1])
                || redirect_partition_index(pair[1].0) != partition
            {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "dirty migration extent is not contiguous"
                );
            }
        }
        Ok(Self {
            fs_id: first_key.fs_id,
            inode: first_key.inode,
            logical_start: first_key.page_index,
            pages: entries.into_iter().map(|(_, handle)| handle).collect(),
            partition,
        })
    }

    pub(crate) fn logical_start(&self) -> u64 {
        self.logical_start
    }

    pub(crate) fn fs_id(&self) -> u64 {
        self.fs_id
    }

    pub(crate) fn inode(&self) -> u64 {
        self.inode
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn nv_base_slot(&self) -> u64 {
        self.pages[0]
            .reference
            .slot()
            .expect("a dirty redirect has an NVPage slot")
    }

    pub(crate) fn pages(&self) -> &[DescriptorHandle] {
        &self.pages
    }

    fn start_key(&self) -> PageKey {
        PageKey::new(self.fs_id, self.inode, self.logical_start)
    }

    fn contains(&self, key: PageKey) -> bool {
        self.fs_id == key.fs_id
            && self.inode == key.inode
            && key.page_index >= self.logical_start
            && key.page_index < self.logical_start + self.pages.len() as u64
    }
}

#[derive(Clone, Debug)]
struct IndexedDirtyExtent {
    extent: DirtyMigrationExtent,
    generation: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct MigrationExtentLease {
    extent: DirtyMigrationExtent,
    generation: u64,
}

impl MigrationExtentLease {
    pub(crate) fn extent(&self) -> &DirtyMigrationExtent {
        &self.extent
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

fn dirty_entries_are_contiguous(
    previous: (PageKey, DescriptorHandle),
    next: (PageKey, DescriptorHandle),
) -> bool {
    previous.0.fs_id == next.0.fs_id
        && previous.0.inode == next.0.inode
        && previous.0.page_index.checked_add(1) == Some(next.0.page_index)
        && previous
            .1
            .reference
            .slot()
            .and_then(|slot| slot.checked_add(1))
            == next.1.reference.slot()
        && previous.1.state == NvObjectState::NvDirty
        && next.1.state == NvObjectState::NvDirty
        && previous.1.migration_target == next.1.migration_target
        && ((previous.1.ssd_target == 0 && next.1.ssd_target == 0)
            || previous
                .1
                .ssd_target
                .checked_add(1)
                .is_some_and(|target| target == next.1.ssd_target))
}

fn build_dirty_migration_extents(
    entries: Vec<(PageKey, DescriptorHandle)>,
) -> Result<Vec<DirtyMigrationExtent>> {
    let mut extents = Vec::new();
    let mut current = Vec::new();
    for entry in entries {
        let can_append = current.last().copied().is_some_and(|previous| {
            current.len() < MAX_DIRTY_MIGRATION_EXTENT_PAGES
                && dirty_entries_are_contiguous(previous, entry)
                && redirect_partition_index(previous.0) == redirect_partition_index(entry.0)
        });
        if !current.is_empty() && !can_append {
            let partition = redirect_partition_index(current[0].0);
            extents.push(DirtyMigrationExtent::try_new(
                partition,
                core::mem::take(&mut current),
            )?);
        }
        current.push(entry);
    }
    if !current.is_empty() {
        let partition = redirect_partition_index(current[0].0);
        extents.push(DirtyMigrationExtent::try_new(partition, current)?);
    }
    Ok(extents)
}

fn redirect_partition_index(key: PageKey) -> usize {
    let owner = key
        .fs_id
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(17)
        ^ key.inode.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (owner.wrapping_add(key.page_index / REDIRECT_PARTITION_PAGES)
        % REDIRECT_PARTITION_COUNT as u64) as usize
}

#[derive(Debug)]
pub(crate) struct PendingRedirectBatch {
    reservations: Vec<(PageKey, u64)>,
}

impl PendingRedirectBatch {
    pub(crate) fn logical_version(&self, key: PageKey) -> Option<u64> {
        self.reservations
            .iter()
            .find_map(|(reserved, version)| (*reserved == key).then_some(*version))
    }
}

#[derive(Default)]
struct RedirectPartition {
    committed: BTreeMap<PageKey, DescriptorHandle>,
    pending: BTreeMap<PageKey, BTreeSet<u64>>,
    dirty: BTreeSet<PageKey>,
    dirty_extents: BTreeMap<PageKey, IndexedDirtyExtent>,
    ready_extents: VecDeque<PageKey>,
    claimed_extents: BTreeMap<PageKey, IndexedDirtyExtent>,
    next_extent_generation: u64,
    epoch: u64,
}

pub(crate) struct RedirectDirectory {
    partitions: Box<[SpinLock<RedirectPartition>]>,
    next_candidate_partition: AtomicUsize,
    candidate_extent_visits: AtomicUsize,
}

impl RedirectDirectory {
    pub(crate) fn new() -> Result<Self> {
        let mut partitions = Vec::new();
        partitions
            .try_reserve_exact(REDIRECT_PARTITION_COUNT)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..REDIRECT_PARTITION_COUNT {
            partitions.push(SpinLock::new(RedirectPartition::default()));
        }
        Ok(Self {
            partitions: partitions.into_boxed_slice(),
            next_candidate_partition: AtomicUsize::new(0),
            candidate_extent_visits: AtomicUsize::new(0),
        })
    }

    pub(crate) fn lookup(&self, key: PageKey) -> Option<DescriptorHandle> {
        self.partitions[self.partition_index(key)]
            .lock()
            .committed
            .get(&key)
            .copied()
    }

    pub(crate) fn reserve_versions(&self, keys: &[PageKey]) -> Result<PendingRedirectBatch> {
        let unique: BTreeSet<_> = keys.iter().copied().collect();
        if unique.len() != keys.len() {
            return_errno_with_message!(Errno::EINVAL, "duplicate redirect publication key");
        }
        let partition_ids = self.partitions_for(keys);
        let mut guards = Vec::new();
        guards
            .try_reserve_exact(partition_ids.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for partition in partition_ids {
            guards.push((partition, self.partitions[partition].lock()));
        }

        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(keys.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for key in keys {
            let partition = self.partition_index(*key);
            let state = guards
                .iter()
                .find(|(index, _)| *index == partition)
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            if state
                .claimed_extents
                .values()
                .any(|indexed| indexed.extent.contains(*key))
            {
                return_errno_with_message!(Errno::EBUSY, "redirect range is migrating");
            }
            if state.pending.contains_key(key) {
                return_errno_with_message!(
                    Errno::EBUSY,
                    "redirect key already has a pending publication"
                );
            }
            let committed = state
                .committed
                .get(key)
                .map(|handle| handle.logical_version)
                .unwrap_or(0);
            let pending = state
                .pending
                .get(key)
                .and_then(|versions| versions.last().copied())
                .unwrap_or(0);
            let version = committed
                .max(pending)
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            reservations.push((*key, version));
        }
        for (key, version) in &reservations {
            let partition = self.partition_index(*key);
            let state = guards
                .iter_mut()
                .find(|(index, _)| *index == partition)
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            state.pending.entry(*key).or_default().insert(*version);
        }
        Ok(PendingRedirectBatch { reservations })
    }

    pub(crate) fn abort_reserved(&self, batch: &PendingRedirectBatch) -> Result<()> {
        let keys: Vec<_> = batch.reservations.iter().map(|(key, _)| *key).collect();
        let partition_ids = self.partitions_for(&keys);
        let mut guards = Vec::new();
        for partition in partition_ids {
            guards.push((partition, self.partitions[partition].lock()));
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter()
                .find(|(index, _)| *index == self.partition_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            if !state
                .pending
                .get(key)
                .is_some_and(|versions| versions.contains(version))
            {
                return_errno!(Errno::ESTALE);
            }
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter_mut()
                .find(|(index, _)| *index == self.partition_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            let versions = state.pending.get_mut(key).expect("reservation was checked");
            versions.remove(version);
            if versions.is_empty() {
                state.pending.remove(key);
            }
        }
        Ok(())
    }

    pub(crate) fn commit_reserved(
        &self,
        batch: &PendingRedirectBatch,
        updates: &[(PageKey, DescriptorHandle)],
    ) -> Result<()> {
        if updates.len() != batch.reservations.len() {
            return_errno!(Errno::EINVAL);
        }
        let update_map: BTreeMap<_, _> = updates.iter().copied().collect();
        if update_map.len() != updates.len() {
            return_errno_with_message!(Errno::EINVAL, "duplicate redirect commit key");
        }
        let keys: Vec<_> = batch.reservations.iter().map(|(key, _)| *key).collect();
        let partition_ids = self.partitions_for(&keys);
        let mut guards = Vec::new();
        for partition in partition_ids {
            guards.push((partition, self.partitions[partition].lock()));
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter()
                .find(|(index, _)| *index == self.partition_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            let handle = update_map
                .get(key)
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if handle.logical_version != *version
                || !state
                    .pending
                    .get(key)
                    .is_some_and(|versions| versions.contains(version))
            {
                return_errno!(Errno::ESTALE);
            }
            if state
                .committed
                .get(key)
                .is_some_and(|current| current.logical_version == *version && current != handle)
            {
                return_errno!(Errno::EUCLEAN);
            }
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter_mut()
                .find(|(index, _)| *index == self.partition_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            let versions = state.pending.get_mut(key).expect("reservation was checked");
            versions.remove(version);
            if versions.is_empty() {
                state.pending.remove(key);
            }
            let handle = *update_map.get(key).expect("commit update was checked");
            if state
                .committed
                .get(key)
                .is_none_or(|current| current.logical_version < handle.logical_version)
            {
                state.committed.insert(*key, handle);
                Self::set_dirty(state, *key, handle.state);
                state.epoch = state.epoch.wrapping_add(1);
            }
        }
        for (partition, state) in &mut guards {
            Self::rebuild_partition_dirty_extents(state, *partition)?;
        }
        Ok(())
    }

    pub(crate) fn install_committed(&self, key: PageKey, handle: DescriptorHandle) -> Result<bool> {
        let mut state = self.partitions[self.partition_index(key)].lock();
        if state
            .claimed_extents
            .values()
            .any(|indexed| indexed.extent.contains(key))
        {
            return_errno_with_message!(Errno::EBUSY, "redirect range is migrating");
        }
        if let Some(current) = state.committed.get(&key) {
            if current.logical_version > handle.logical_version {
                return Ok(false);
            }
            if current.logical_version == handle.logical_version {
                if *current == handle {
                    return Ok(false);
                }
                return_errno!(Errno::EUCLEAN);
            }
        }
        state.committed.insert(key, handle);
        Self::set_dirty(&mut state, key, handle.state);
        state.epoch = state.epoch.wrapping_add(1);
        Self::rebuild_partition_dirty_extents(&mut state, self.partition_index(key))?;
        Ok(true)
    }

    pub(crate) fn compare_update(
        &self,
        key: PageKey,
        expected: DescriptorHandle,
        replacement: DescriptorHandle,
    ) -> bool {
        if expected.reference != replacement.reference
            || expected.logical_version != replacement.logical_version
        {
            return false;
        }
        let mut state = self.partitions[self.partition_index(key)].lock();
        if state.committed.get(&key) != Some(&expected) {
            return false;
        }
        state.committed.insert(key, replacement);
        Self::set_dirty(&mut state, key, replacement.state);
        state.epoch = state.epoch.wrapping_add(1);
        Self::rebuild_partition_dirty_extents(&mut state, self.partition_index(key))
            .expect("committed redirects form valid dirty extents");
        true
    }

    pub(crate) fn compare_remove(&self, key: PageKey, expected: DescriptorHandle) -> bool {
        let mut state = self.partitions[self.partition_index(key)].lock();
        if state.committed.get(&key) != Some(&expected) {
            return false;
        }
        state.committed.remove(&key);
        state.dirty.remove(&key);
        state.epoch = state.epoch.wrapping_add(1);
        Self::rebuild_partition_dirty_extents(&mut state, self.partition_index(key))
            .expect("committed redirects form valid dirty extents");
        true
    }

    pub(crate) fn rebuild<I>(&self, entries: I) -> Result<()>
    where
        I: IntoIterator<Item = (PageKey, DescriptorHandle)>,
    {
        let mut rebuilt = BTreeMap::new();
        for (key, handle) in entries {
            if rebuilt.insert(key, handle).is_some() {
                return_errno_with_message!(Errno::EUCLEAN, "duplicate rebuilt redirect key");
            }
        }
        let mut guards = Vec::new();
        for partition in 0..self.partitions.len() {
            guards.push(self.partitions[partition].lock());
        }
        for state in &mut guards {
            state.committed.clear();
            state.pending.clear();
            state.dirty.clear();
            state.dirty_extents.clear();
            state.ready_extents.clear();
            state.claimed_extents.clear();
            state.epoch = state.epoch.wrapping_add(1);
        }
        for (key, handle) in rebuilt {
            let state = &mut guards[self.partition_index(key)];
            state.committed.insert(key, handle);
            Self::set_dirty(state, key, handle.state);
        }
        for (partition, state) in guards.iter_mut().enumerate() {
            Self::rebuild_partition_dirty_extents(state, partition)?;
        }
        Ok(())
    }

    pub(crate) fn claim_ready_extents(
        &self,
        fs_id: u64,
        page_budget: usize,
    ) -> Result<Vec<MigrationExtentLease>> {
        self.candidate_extent_visits.store(0, Ordering::Relaxed);
        if page_budget == 0 {
            return Ok(Vec::new());
        }
        let start = self
            .next_candidate_partition
            .fetch_add(1, Ordering::Relaxed)
            % self.partitions.len();
        let mut remaining = page_budget;
        let mut leases = Vec::new();
        for offset in 0..self.partitions.len() {
            let partition = (start + offset) % self.partitions.len();
            let mut state = self.partitions[partition].lock();
            let queued = state.ready_extents.len();
            for _ in 0..queued {
                let Some(start_key) = state.ready_extents.pop_front() else {
                    break;
                };
                let Some(indexed) = state.dirty_extents.get(&start_key) else {
                    continue;
                };
                self.candidate_extent_visits.fetch_add(1, Ordering::Relaxed);
                if indexed.extent.fs_id() != fs_id || indexed.extent.len() > remaining {
                    state.ready_extents.push_back(start_key);
                    continue;
                }
                let indexed = state
                    .dirty_extents
                    .remove(&start_key)
                    .expect("ready extent was checked");
                remaining -= indexed.extent.len();
                state.claimed_extents.insert(start_key, indexed.clone());
                leases.push(MigrationExtentLease {
                    extent: indexed.extent,
                    generation: indexed.generation,
                });
                if remaining == 0 {
                    return Ok(leases);
                }
            }
        }
        Ok(leases)
    }

    pub(crate) fn requeue_extent(&self, lease: MigrationExtentLease) -> Result<()> {
        let partition = lease.extent.partition;
        let start_key = lease.extent.start_key();
        let mut state = self.partitions[partition].lock();
        let Some(indexed) = state.claimed_extents.get(&start_key) else {
            return_errno!(Errno::ESTALE);
        };
        if indexed.generation != lease.generation || indexed.extent != lease.extent {
            return_errno!(Errno::ESTALE);
        }
        let indexed = state
            .claimed_extents
            .remove(&start_key)
            .expect("claimed extent was checked");
        state.dirty_extents.insert(start_key, indexed);
        state.ready_extents.push_back(start_key);
        Ok(())
    }

    pub(crate) fn validate_claimed_extent(&self, lease: &MigrationExtentLease) -> bool {
        let partition = lease.extent.partition;
        let start_key = lease.extent.start_key();
        let state = self.partitions[partition].lock();
        let Some(indexed) = state.claimed_extents.get(&start_key) else {
            return false;
        };
        if indexed.generation != lease.generation || indexed.extent != lease.extent {
            return false;
        }
        lease
            .extent
            .pages
            .iter()
            .enumerate()
            .all(|(offset, handle)| {
                let key = PageKey::new(
                    lease.extent.fs_id,
                    lease.extent.inode,
                    lease.extent.logical_start + offset as u64,
                );
                state.committed.get(&key) == Some(handle)
            })
    }

    pub(crate) fn claimed_extent_overlaps(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> bool {
        let Some(logical_end) = logical_start.checked_add(pages as u64) else {
            return true;
        };
        self.partitions.iter().any(|partition| {
            partition.lock().claimed_extents.values().any(|indexed| {
                let extent = &indexed.extent;
                let extent_end = extent.logical_start + extent.pages.len() as u64;
                extent.fs_id == fs_id
                    && extent.inode == inode
                    && extent.logical_start < logical_end
                    && logical_start < extent_end
            })
        })
    }

    #[cfg(ktest)]
    fn candidate_extent_visits_for_test(&self) -> usize {
        self.candidate_extent_visits.load(Ordering::Relaxed)
    }

    pub(crate) fn dirty_ranges(&self, budget: usize) -> Vec<RedirectRange> {
        if budget == 0 {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for (partition, lock) in self.partitions.iter().enumerate() {
            let state = lock.lock();
            for key in &state.dirty {
                if let Some(handle) = state.committed.get(key).copied() {
                    candidates.push((*key, handle, partition, state.epoch));
                }
            }
        }
        candidates.sort_unstable_by_key(|(key, _, _, _)| *key);

        let mut ranges = Vec::<RedirectRange>::new();
        let mut accepted = 0;
        for (key, handle, partition, epoch) in candidates {
            if accepted == budget {
                break;
            }
            let merge = ranges.last().is_some_and(|range| {
                let previous = *range.pages.last().expect("redirect range is nonempty");
                range.fs_id == key.fs_id
                    && range.inode == key.inode
                    && range.partition == partition
                    && range.epoch == epoch
                    && range.logical_start + range.pages.len() as u64 == key.page_index
                    && previous
                        .reference
                        .slot()
                        .and_then(|slot| slot.checked_add(1))
                        == handle.reference.slot()
                    && previous.state == handle.state
                    && ((previous.ssd_target == 0 && handle.ssd_target == 0)
                        || previous
                            .ssd_target
                            .checked_add(1)
                            .is_some_and(|target| target == handle.ssd_target))
            });
            if merge {
                ranges
                    .last_mut()
                    .expect("merge requires a preceding redirect range")
                    .pages
                    .push(handle);
            } else {
                ranges.push(RedirectRange {
                    fs_id: key.fs_id,
                    inode: key.inode,
                    logical_start: key.page_index,
                    pages: vec![handle],
                    epoch,
                    partition,
                });
            }
            accepted += 1;
        }
        ranges
    }

    fn set_dirty(state: &mut RedirectPartition, key: PageKey, object_state: NvObjectState) {
        if object_state == NvObjectState::NvDirty {
            state.dirty.insert(key);
        } else {
            state.dirty.remove(&key);
        }
    }

    fn rebuild_partition_dirty_extents(
        state: &mut RedirectPartition,
        partition: usize,
    ) -> Result<()> {
        let entries = state
            .dirty
            .iter()
            .filter(|key| {
                !state
                    .claimed_extents
                    .values()
                    .any(|indexed| indexed.extent.contains(**key))
            })
            .filter_map(|key| {
                state
                    .committed
                    .get(key)
                    .copied()
                    .map(|handle| (*key, handle))
            })
            .collect();
        let extents = build_dirty_migration_extents(entries)?;
        state.dirty_extents.clear();
        state.ready_extents.clear();
        for extent in extents {
            if extent.partition != partition {
                return_errno_with_message!(Errno::EUCLEAN, "dirty extent partition mismatch");
            }
            state.next_extent_generation = state
                .next_extent_generation
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let start_key = extent.start_key();
            state.dirty_extents.insert(
                start_key,
                IndexedDirtyExtent {
                    extent,
                    generation: state.next_extent_generation,
                },
            );
            state.ready_extents.push_back(start_key);
        }
        Ok(())
    }

    fn partitions_for(&self, keys: &[PageKey]) -> Vec<usize> {
        keys.iter()
            .map(|key| self.partition_index(*key))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn partition_index(&self, key: PageKey) -> usize {
        redirect_partition_index(key)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    fn handle(
        slot: u64,
        allocation_seq: u64,
        logical_version: u64,
        target: u64,
    ) -> DescriptorHandle {
        DescriptorHandle {
            reference: PersistentObjectRef::new(slot, allocation_seq).unwrap(),
            logical_version,
            state: NvObjectState::NvDirty,
            ssd_target: target,
            migration_target: false,
        }
    }

    fn extent_entries(
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        nv_slot_start: u64,
        pages: usize,
    ) -> Vec<(PageKey, DescriptorHandle)> {
        (0..pages)
            .map(|offset| {
                let offset = offset as u64;
                (
                    PageKey::new(fs_id, inode, logical_start + offset),
                    handle(
                        nv_slot_start + offset,
                        1000 + offset,
                        1,
                        10_000 + logical_start + offset,
                    ),
                )
            })
            .collect()
    }

    fn dirty_migration_extent_accepts_one_and_128_pages() {
        for pages in [1, 128] {
            let entries = extent_entries(1, 2, 0, 100, pages);
            let partition = redirect_partition_index(entries[0].0);
            let extent = DirtyMigrationExtent::try_new(partition, entries).unwrap();
            assert_eq!(extent.logical_start(), 0);
            assert_eq!(extent.len(), pages);
            assert_eq!(extent.nv_base_slot(), 100);
        }
    }

    fn dirty_migration_extent_rejects_zero_and_129_pages() {
        assert_eq!(
            DirtyMigrationExtent::try_new(0, Vec::new())
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        let entries = extent_entries(1, 2, 0, 100, 129);
        let partition = redirect_partition_index(entries[0].0);
        assert_eq!(
            DirtyMigrationExtent::try_new(partition, entries)
                .unwrap_err()
                .error(),
            Errno::E2BIG
        );
    }

    fn dirty_migration_extent_rejects_discontinuities() {
        let mut logical = extent_entries(1, 2, 0, 100, 3);
        logical[1].0 = PageKey::new(1, 2, 7);
        let partition = redirect_partition_index(logical[0].0);
        assert!(DirtyMigrationExtent::try_new(partition, logical).is_err());

        let mut physical = extent_entries(1, 2, 0, 100, 3);
        physical[1].1.reference = PersistentObjectRef::new(900, 1001).unwrap();
        let partition = redirect_partition_index(physical[0].0);
        assert!(DirtyMigrationExtent::try_new(partition, physical).is_err());

        let mut owner = extent_entries(1, 2, 0, 100, 3);
        owner[1].0 = PageKey::new(1, 3, 1);
        let partition = redirect_partition_index(owner[0].0);
        assert!(DirtyMigrationExtent::try_new(partition, owner).is_err());
    }

    fn dirty_migration_extent_caps_merge_and_splits_on_a_hole() {
        let entries = extent_entries(1, 2, 0, 100, 256);
        let extents = build_dirty_migration_extents(entries.clone()).unwrap();
        assert_eq!(
            extents
                .iter()
                .map(DirtyMigrationExtent::len)
                .collect::<Vec<_>>(),
            vec![128, 128]
        );

        let first_extent: Vec<_> = entries.iter().copied().take(128).collect();
        let without_middle = first_extent
            .iter()
            .copied()
            .filter(|(key, _)| key.page_index != 64)
            .collect();
        let split = build_dirty_migration_extents(without_middle).unwrap();
        assert_eq!(
            split
                .iter()
                .map(DirtyMigrationExtent::len)
                .collect::<Vec<_>>(),
            vec![64, 63]
        );

        let rebuilt = build_dirty_migration_extents(first_extent).unwrap();
        assert_eq!(
            rebuilt
                .iter()
                .map(DirtyMigrationExtent::len)
                .collect::<Vec<_>>(),
            vec![128]
        );
    }

    fn dirty_migration_extent_rejects_partition_crossing() {
        let entries = extent_entries(1, 2, REDIRECT_PARTITION_PAGES - 1, 100, 2);
        let partition = redirect_partition_index(entries[0].0);
        assert_ne!(partition, redirect_partition_index(entries[1].0));
        assert!(DirtyMigrationExtent::try_new(partition, entries).is_err());
    }

    #[ktest]
    fn rnvpce_dirty_migration_extents() {
        dirty_migration_extent_accepts_one_and_128_pages();
        dirty_migration_extent_rejects_zero_and_129_pages();
        dirty_migration_extent_rejects_discontinuities();
        dirty_migration_extent_caps_merge_and_splits_on_a_hole();
        dirty_migration_extent_rejects_partition_crossing();
    }

    fn dirty_extent_index_claims_without_page_scan_or_split() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .rebuild(extent_entries(0x55, 7, 0, 1000, 256))
            .unwrap();

        assert!(directory.claim_ready_extents(0x55, 127).unwrap().is_empty());
        let mut first = directory.claim_ready_extents(0x55, 128).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].extent().len(), 128);
        assert_eq!(directory.candidate_extent_visits_for_test(), 1);

        let second = directory.claim_ready_extents(0x55, 128).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].extent().logical_start(), 128);
        assert!(directory.claim_ready_extents(0x55, 128).unwrap().is_empty());

        directory.requeue_extent(first.remove(0)).unwrap();
        let retried = directory.claim_ready_extents(0x55, 128).unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].extent().logical_start(), 0);
    }

    fn dirty_extent_index_updates_only_affected_partition() {
        let directory = RedirectDirectory::new().unwrap();
        for page in 0..3 {
            directory
                .install_committed(
                    PageKey::new(0x66, 8, page),
                    handle(2000 + page, 3000 + page, 1, 4000 + page),
                )
                .unwrap();
        }
        let claimed = directory.claim_ready_extents(0x66, 128).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].extent().len(), 3);
        assert_eq!(directory.candidate_extent_visits_for_test(), 1);
    }

    fn dirty_extent_recovery_rebuild_is_order_independent() {
        let directory = RedirectDirectory::new().unwrap();
        let mut shuffled = extent_entries(0x77, 9, 0, 5000, 128);
        shuffled.reverse();
        directory.rebuild(shuffled).unwrap();
        let claimed = directory.claim_ready_extents(0x77, 128).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].extent().logical_start(), 0);
        assert_eq!(claimed[0].extent().len(), 128);
    }

    fn dirty_extent_lease_blocks_only_its_logical_range() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .rebuild(extent_entries(0x88, 10, 0, 6000, 4))
            .unwrap();
        let lease = directory.claim_ready_extents(0x88, 4).unwrap().remove(0);
        assert_eq!(
            directory
                .reserve_versions(&[PageKey::new(0x88, 10, 2)])
                .unwrap_err()
                .error(),
            Errno::EBUSY
        );
        let outside = directory
            .reserve_versions(&[PageKey::new(0x88, 10, 8)])
            .unwrap();
        directory.abort_reserved(&outside).unwrap();
        directory.requeue_extent(lease).unwrap();
    }

    #[ktest]
    fn rnvpce_dirty_extent_index() {
        dirty_extent_index_claims_without_page_scan_or_split();
        dirty_extent_index_updates_only_affected_partition();
        dirty_extent_recovery_rebuild_is_order_independent();
        dirty_extent_lease_blocks_only_its_logical_range();
    }

    #[ktest]
    fn redirect_reservations_serialize_the_same_page() {
        let directory = RedirectDirectory::new().unwrap();
        let key = PageKey::new(1, 2, 3);
        let first = directory.reserve_versions(&[key]).unwrap();
        assert_eq!(first.logical_version(key), Some(1));
        assert_eq!(
            directory.reserve_versions(&[key]).unwrap_err().error(),
            Errno::EBUSY
        );

        directory.abort_reserved(&first).unwrap();
        let second = directory.reserve_versions(&[key]).unwrap();
        directory
            .commit_reserved(&second, &[(key, handle(11, 101, 1, 1003))])
            .unwrap();
        assert_eq!(directory.lookup(key), Some(handle(11, 101, 1, 1003)));
        let third = directory.reserve_versions(&[key]).unwrap();
        assert_eq!(third.logical_version(key), Some(2));
    }

    #[ktest]
    fn redirect_ranges_merge_only_compatible_contiguous_pages() {
        let directory = RedirectDirectory::new().unwrap();
        let entries: Vec<_> = (0..32)
            .map(|page| {
                (
                    PageKey::new(7, 8, page),
                    handle(100 + page, 1000 + page * 3, 1, 10_000 + page),
                )
            })
            .collect();
        directory.rebuild(entries.clone()).unwrap();

        let ranges = directory.dirty_ranges(64);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].logical_start, 0);
        assert_eq!(ranges[0].pages.len(), 32);
        for (page, descriptor) in ranges[0].pages.iter().enumerate() {
            assert_eq!(descriptor.reference.slot(), Some(100 + page as u64));
            assert_eq!(
                descriptor.reference.allocation_seq(),
                1000 + page as u64 * 3
            );
        }

        let discontinuous = handle(500, 2000, 2, 20_000);
        directory
            .install_committed(PageKey::new(7, 8, 16), discontinuous)
            .unwrap();
        let ranges = directory.dirty_ranges(64);
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.pages.len())
                .collect::<Vec<_>>(),
            vec![16, 1, 15]
        );
    }

    #[ktest]
    fn redirect_partial_overwrite_splits_only_dram_ranges() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .rebuild((0..8).map(|page| {
                (
                    PageKey::new(9, 10, page),
                    handle(200 + page, 300 + page, 1, 400 + page),
                )
            }))
            .unwrap();

        directory
            .install_committed(PageKey::new(9, 10, 4), handle(900, 901, 2, 404))
            .unwrap();
        assert_eq!(
            directory
                .dirty_ranges(16)
                .iter()
                .map(|range| range.pages.len())
                .collect::<Vec<_>>(),
            vec![4, 1, 3]
        );
    }

    #[ktest]
    fn redirect_exact_remove_preserves_newer_generation() {
        let directory = RedirectDirectory::new().unwrap();
        let key = PageKey::new(11, 12, 13);
        let old = handle(10, 20, 1, 30);
        let new = handle(11, 21, 2, 30);
        directory.install_committed(key, old).unwrap();
        directory.install_committed(key, new).unwrap();

        assert!(!directory.compare_remove(key, old));
        assert_eq!(directory.lookup(key), Some(new));
        assert!(directory.compare_remove(key, new));
        assert_eq!(directory.lookup(key), None);
    }

    #[ktest]
    fn redirect_dirty_range_splits_at_partition_and_budget_boundaries() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .rebuild((250..270).map(|page| {
                (
                    PageKey::new(13, 14, page),
                    handle(1000 + page, 2000 + page, 1, 3000 + page),
                )
            }))
            .unwrap();

        let ranges = directory.dirty_ranges(10);
        assert_eq!(
            ranges.iter().map(|range| range.pages.len()).sum::<usize>(),
            10
        );
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.pages.len())
                .collect::<Vec<_>>(),
            vec![6, 4]
        );
        let pages: Vec<_> = ranges
            .iter()
            .flat_map(|range| {
                (0..range.pages.len()).map(move |offset| range.logical_start + offset as u64)
            })
            .collect();
        assert_eq!(pages, (250..260).collect::<Vec<_>>());
    }
}
