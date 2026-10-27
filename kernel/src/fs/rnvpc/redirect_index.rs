// SPDX-License-Identifier: MPL-2.0

//! Volatile exact-page redirect and contiguous-page-range indexes for rNVPCe.

use super::{descriptor::NvObjectState, index::PageKey, metadata::PersistentObjectRef};
use crate::prelude::*;

const REDIRECT_SHARD_COUNT: usize = 64;
const REDIRECT_SHARD_PAGES: u64 = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DescriptorHandle {
    pub(crate) reference: PersistentObjectRef,
    pub(crate) logical_version: u64,
    pub(crate) state: NvObjectState,
    pub(crate) ssd_target: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RedirectRange {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) logical_start: u64,
    pub(crate) pages: Vec<DescriptorHandle>,
    pub(crate) epoch: u64,
    shard: usize,
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
struct RedirectShard {
    committed: BTreeMap<PageKey, DescriptorHandle>,
    pending: BTreeMap<PageKey, BTreeSet<u64>>,
    dirty: BTreeSet<PageKey>,
    epoch: u64,
}

pub(crate) struct RedirectDirectory {
    shards: Box<[SpinLock<RedirectShard>]>,
}

impl RedirectDirectory {
    pub(crate) fn new() -> Result<Self> {
        let mut shards = Vec::new();
        shards
            .try_reserve_exact(REDIRECT_SHARD_COUNT)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..REDIRECT_SHARD_COUNT {
            shards.push(SpinLock::new(RedirectShard::default()));
        }
        Ok(Self {
            shards: shards.into_boxed_slice(),
        })
    }

    pub(crate) fn lookup(&self, key: PageKey) -> Option<DescriptorHandle> {
        self.shards[self.shard_index(key)]
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
        let shard_ids = self.shards_for(keys);
        let mut guards = Vec::new();
        guards
            .try_reserve_exact(shard_ids.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for shard in shard_ids {
            guards.push((shard, self.shards[shard].lock()));
        }

        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(keys.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for key in keys {
            let shard = self.shard_index(*key);
            let state = guards
                .iter()
                .find(|(index, _)| *index == shard)
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
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
            let shard = self.shard_index(*key);
            let state = guards
                .iter_mut()
                .find(|(index, _)| *index == shard)
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
            state.pending.entry(*key).or_default().insert(*version);
        }
        Ok(PendingRedirectBatch { reservations })
    }

    pub(crate) fn abort_reserved(&self, batch: &PendingRedirectBatch) -> Result<()> {
        let keys: Vec<_> = batch.reservations.iter().map(|(key, _)| *key).collect();
        let shard_ids = self.shards_for(&keys);
        let mut guards = Vec::new();
        for shard in shard_ids {
            guards.push((shard, self.shards[shard].lock()));
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter()
                .find(|(index, _)| *index == self.shard_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
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
                .find(|(index, _)| *index == self.shard_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
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
        let shard_ids = self.shards_for(&keys);
        let mut guards = Vec::new();
        for shard in shard_ids {
            guards.push((shard, self.shards[shard].lock()));
        }
        for (key, version) in &batch.reservations {
            let state = guards
                .iter()
                .find(|(index, _)| *index == self.shard_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
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
                .find(|(index, _)| *index == self.shard_index(*key))
                .map(|(_, state)| state)
                .expect("requested redirect shard is locked");
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
                state.epoch = state
                    .epoch
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            }
        }
        Ok(())
    }

    pub(crate) fn install_committed(&self, key: PageKey, handle: DescriptorHandle) -> Result<bool> {
        let mut state = self.shards[self.shard_index(key)].lock();
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
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(true)
    }

    pub(crate) fn compare_remove(&self, key: PageKey, expected: DescriptorHandle) -> bool {
        let mut state = self.shards[self.shard_index(key)].lock();
        if state.committed.get(&key) != Some(&expected) {
            return false;
        }
        state.committed.remove(&key);
        state.dirty.remove(&key);
        state.epoch = state.epoch.wrapping_add(1);
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
        for shard in 0..self.shards.len() {
            guards.push(self.shards[shard].lock());
        }
        for state in &mut guards {
            state.committed.clear();
            state.pending.clear();
            state.dirty.clear();
            state.epoch = state.epoch.wrapping_add(1);
        }
        for (key, handle) in rebuilt {
            let state = &mut guards[self.shard_index(key)];
            state.committed.insert(key, handle);
            Self::set_dirty(state, key, handle.state);
        }
        Ok(())
    }

    pub(crate) fn dirty_ranges(&self, budget: usize) -> Vec<RedirectRange> {
        if budget == 0 {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for (shard, lock) in self.shards.iter().enumerate() {
            let state = lock.lock();
            for key in &state.dirty {
                if let Some(handle) = state.committed.get(key).copied() {
                    candidates.push((*key, handle, shard, state.epoch));
                }
            }
        }
        candidates.sort_unstable_by_key(|(key, _, _, _)| *key);

        let mut ranges = Vec::<RedirectRange>::new();
        let mut accepted = 0;
        for (key, handle, shard, epoch) in candidates {
            if accepted == budget {
                break;
            }
            let merge = ranges.last().is_some_and(|range| {
                let previous = *range.pages.last().expect("redirect range is nonempty");
                range.fs_id == key.fs_id
                    && range.inode == key.inode
                    && range.shard == shard
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
                    shard,
                });
            }
            accepted += 1;
        }
        ranges
    }

    fn set_dirty(state: &mut RedirectShard, key: PageKey, object_state: NvObjectState) {
        if object_state == NvObjectState::NvDirty {
            state.dirty.insert(key);
        } else {
            state.dirty.remove(&key);
        }
    }

    fn shards_for(&self, keys: &[PageKey]) -> Vec<usize> {
        keys.iter()
            .map(|key| self.shard_index(*key))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn shard_index(&self, key: PageKey) -> usize {
        let owner = key
            .fs_id
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(17)
            ^ key.inode.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        (owner.wrapping_add(key.page_index / REDIRECT_SHARD_PAGES) % REDIRECT_SHARD_COUNT as u64)
            as usize
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
        }
    }

    #[ktest]
    fn redirect_reservations_are_monotonic_while_persist_is_unlocked() {
        let directory = RedirectDirectory::new().unwrap();
        let key = PageKey::new(1, 2, 3);
        let first = directory.reserve_versions(&[key]).unwrap();
        let second = directory.reserve_versions(&[key]).unwrap();
        assert_eq!(first.logical_version(key), Some(1));
        assert_eq!(second.logical_version(key), Some(2));

        directory.abort_reserved(&first).unwrap();
        directory
            .commit_reserved(&second, &[(key, handle(11, 101, 2, 1003))])
            .unwrap();
        assert_eq!(directory.lookup(key), Some(handle(11, 101, 2, 1003)));
        let third = directory.reserve_versions(&[key]).unwrap();
        assert_eq!(third.logical_version(key), Some(3));
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
    fn redirect_dirty_range_splits_at_shard_and_budget_boundaries() {
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
