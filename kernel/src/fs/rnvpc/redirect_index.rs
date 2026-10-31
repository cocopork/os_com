// SPDX-License-Identifier: MPL-2.0

//! Volatile exact-page redirect and contiguous-page-range indexes for rNVPCe.

use core::{
    ops::Range,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};

use ostd::sync::WaitQueue;

use super::{
    descriptor::{NvObjectState, NvRedirectAuthority, NvRedirectExtentRecord},
    index::PageKey,
    metadata::PersistentObjectRef,
};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum NvExtentEntryState {
    Publishing = 1,
    Dirty = 2,
    Migrating = 3,
    IoComplete = 4,
    Reclaiming = 5,
    Reclaimed = 6,
}

impl NvExtentEntryState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Publishing,
            2 => Self::Dirty,
            3 => Self::Migrating,
            4 => Self::IoComplete,
            5 => Self::Reclaiming,
            6 => Self::Reclaimed,
            _ => panic!("invalid NV extent entry state"),
        }
    }
}

pub(crate) struct DirtyNvExtentEntry {
    record: NvRedirectExtentRecord,
    state: AtomicU8,
    readers: AtomicUsize,
    writers: AtomicUsize,
    waiters: WaitQueue,
}

impl DirtyNvExtentEntry {
    pub(crate) fn new(record: NvRedirectExtentRecord) -> Result<Self> {
        Self::new_with_state(record, NvExtentEntryState::Dirty)
    }

    pub(crate) fn new_publishing(record: NvRedirectExtentRecord) -> Result<Self> {
        Self::new_with_state(record, NvExtentEntryState::Publishing)
    }

    fn new_with_state(record: NvRedirectExtentRecord, state: NvExtentEntryState) -> Result<Self> {
        record.validate()?;
        if record.authority()? != NvRedirectAuthority::NvAuthoritative {
            return_errno_with_message!(Errno::EINVAL, "dirty entry is not NV authoritative");
        }
        Ok(Self {
            record,
            state: AtomicU8::new(state as u8),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) const fn record(&self) -> &NvRedirectExtentRecord {
        &self.record
    }

    pub(crate) fn state(&self) -> NvExtentEntryState {
        NvExtentEntryState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) const fn fs_id(&self) -> u64 {
        self.record.fs_id()
    }

    pub(crate) const fn inode(&self) -> u64 {
        self.record.inode()
    }

    pub(crate) const fn logical_start(&self) -> u64 {
        self.record.logical_start()
    }

    pub(crate) const fn page_count(&self) -> u16 {
        self.record.page_count()
    }

    pub(crate) fn logical_range(&self) -> Range<u64> {
        self.record
            .logical_range()
            .expect("validated dirty entry has a bounded logical range")
    }

    pub(crate) fn nv_slot_range(&self) -> Range<u64> {
        self.record
            .nv_slot_range()
            .expect("validated dirty entry has a bounded NV range")
    }

    pub(crate) fn ssd_block_range(&self) -> Range<u64> {
        self.record
            .ssd_block_range()
            .expect("validated dirty entry has a bounded SSD range")
    }

    fn transition(&self, from: NvExtentEntryState, to: NvExtentEntryState) -> Result<()> {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::new(Errno::ESTALE))?;
        self.waiters.wake_all();
        Ok(())
    }

    fn activate_publication(&self) {
        self.state
            .compare_exchange(
                NvExtentEntryState::Publishing as u8,
                NvExtentEntryState::Dirty as u8,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .expect("a prepared NV extent entry is activated exactly once");
        self.waiters.wake_all();
    }

    pub(crate) fn try_read(self: &Arc<Self>) -> Result<NvExtentReadGuard> {
        loop {
            let state = self.state();
            if !matches!(
                state,
                NvExtentEntryState::Dirty
                    | NvExtentEntryState::Migrating
                    | NvExtentEntryState::IoComplete
            ) {
                return_errno_with_message!(Errno::ESTALE, "NV extent entry is reclaiming");
            }
            self.readers.fetch_add(1, Ordering::AcqRel);
            if matches!(
                self.state(),
                NvExtentEntryState::Dirty
                    | NvExtentEntryState::Migrating
                    | NvExtentEntryState::IoComplete
            ) {
                return Ok(NvExtentReadGuard {
                    entry: self.clone(),
                });
            }
            if self.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.waiters.wake_all();
            }
        }
    }

    pub(crate) fn try_write(self: &Arc<Self>) -> Result<NvExtentWriteGuard> {
        if self.state() != NvExtentEntryState::Dirty {
            return_errno_with_message!(Errno::EBUSY, "NV extent entry is migrating");
        }
        self.writers.fetch_add(1, Ordering::AcqRel);
        if self.state() == NvExtentEntryState::Dirty {
            return Ok(NvExtentWriteGuard {
                entry: self.clone(),
            });
        }
        if self.writers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.waiters.wake_all();
        }
        return_errno_with_message!(Errno::EBUSY, "NV extent entry is migrating")
    }

    fn wait_for_writers(&self) {
        let _ = self
            .waiters
            .wait_until(|| (self.writers.load(Ordering::Acquire) == 0).then_some(()));
    }

    fn begin_reclaim_and_wait_for_readers(&self) -> Result<()> {
        self.transition(
            NvExtentEntryState::IoComplete,
            NvExtentEntryState::Reclaiming,
        )?;
        let _ = self
            .waiters
            .wait_until(|| (self.readers.load(Ordering::Acquire) == 0).then_some(()));
        Ok(())
    }

    fn finish_reclaim(&self) -> Result<()> {
        self.transition(
            NvExtentEntryState::Reclaiming,
            NvExtentEntryState::Reclaimed,
        )
    }
}

pub(crate) struct NvExtentReadGuard {
    entry: Arc<DirtyNvExtentEntry>,
}

impl NvExtentReadGuard {
    pub(crate) fn entry(&self) -> &Arc<DirtyNvExtentEntry> {
        &self.entry
    }

    pub(crate) fn nv_slot(&self, logical_page: u64) -> Result<u64> {
        let offset = logical_page
            .checked_sub(self.entry.logical_start())
            .filter(|offset| *offset < u64::from(self.entry.page_count()))
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        self.entry
            .nv_slot_range()
            .start
            .checked_add(offset)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))
    }
}

impl Drop for NvExtentReadGuard {
    fn drop(&mut self) {
        if self.entry.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.entry.waiters.wake_all();
        }
    }
}

pub(crate) struct NvExtentWriteGuard {
    entry: Arc<DirtyNvExtentEntry>,
}

impl NvExtentWriteGuard {
    pub(crate) fn entry(&self) -> &Arc<DirtyNvExtentEntry> {
        &self.entry
    }
}

impl Drop for NvExtentWriteGuard {
    fn drop(&mut self) {
        if self.entry.writers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.entry.waiters.wake_all();
        }
    }
}

pub(crate) struct NvExtentEntryLease {
    entry: Arc<DirtyNvExtentEntry>,
}

impl NvExtentEntryLease {
    pub(crate) fn entry(&self) -> &Arc<DirtyNvExtentEntry> {
        &self.entry
    }

    pub(crate) fn wait_for_writers(&self) {
        self.entry.wait_for_writers();
    }

    pub(crate) fn begin_reclaim_and_wait_for_readers(&self) -> Result<()> {
        self.entry.begin_reclaim_and_wait_for_readers()
    }

    pub(crate) fn finish_reclaim(&self) -> Result<()> {
        self.entry.finish_reclaim()
    }
}

impl Drop for NvExtentEntryLease {
    fn drop(&mut self) {
        let _ = self.entry.state.compare_exchange(
            NvExtentEntryState::Migrating as u8,
            NvExtentEntryState::Dirty as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.entry.waiters.wake_all();
    }
}

#[cfg(ktest)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MigrationConflict {
    pub(crate) logical_start: u64,
    pub(crate) logical_end: u64,
    pub(crate) generation: u64,
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

fn inode_redirect_partition_index(fs_id: u64, inode: u64) -> usize {
    let owner = fs_id.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17)
        ^ inode.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (owner % REDIRECT_PARTITION_COUNT as u64) as usize
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
    ready_pages: VecDeque<PageKey>,
    ready_set: BTreeSet<PageKey>,
    claimed_extents: BTreeMap<PageKey, IndexedDirtyExtent>,
    next_extent_generation: u64,
    epoch: u64,
}

struct InodeRedirectMap {
    entries: SpinLock<BTreeMap<u64, Arc<DirtyNvExtentEntry>>>,
}

impl InodeRedirectMap {
    fn new() -> Self {
        Self {
            entries: SpinLock::new(BTreeMap::new()),
        }
    }
}

pub(crate) struct RedirectDirectory {
    partitions: Box<[SpinLock<RedirectPartition>]>,
    inode_maps: Box<[SpinLock<BTreeMap<(u64, u64), Arc<InodeRedirectMap>>>]>,
    next_candidate_partition: AtomicUsize,
    candidate_extent_visits: AtomicUsize,
    #[cfg(ktest)]
    overlap_lookups: AtomicUsize,
    #[cfg(ktest)]
    dirty_extent_rebuilds: AtomicUsize,
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
        let mut inode_maps = Vec::new();
        inode_maps
            .try_reserve_exact(REDIRECT_PARTITION_COUNT)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..REDIRECT_PARTITION_COUNT {
            inode_maps.push(SpinLock::new(BTreeMap::new()));
        }
        Ok(Self {
            partitions: partitions.into_boxed_slice(),
            inode_maps: inode_maps.into_boxed_slice(),
            next_candidate_partition: AtomicUsize::new(0),
            candidate_extent_visits: AtomicUsize::new(0),
            #[cfg(ktest)]
            overlap_lookups: AtomicUsize::new(0),
            #[cfg(ktest)]
            dirty_extent_rebuilds: AtomicUsize::new(0),
        })
    }

    pub(crate) fn lookup(&self, key: PageKey) -> Option<DescriptorHandle> {
        self.partitions[self.partition_index(key)]
            .lock()
            .committed
            .get(&key)
            .copied()
    }

    pub(crate) fn publish_dirty_entry(&self, entry: Arc<DirtyNvExtentEntry>) -> Result<()> {
        self.publish_one_entry(entry, NvExtentEntryState::Dirty)
    }

    pub(crate) fn publish_entries(&self, new_entries: &[Arc<DirtyNvExtentEntry>]) -> Result<()> {
        self.prepare_entries(new_entries)?;
        self.activate_prepared_entries(new_entries);
        Ok(())
    }

    pub(crate) fn prepare_entries(&self, new_entries: &[Arc<DirtyNvExtentEntry>]) -> Result<()> {
        for entry in new_entries {
            if entry.state() != NvExtentEntryState::Publishing {
                return_errno_with_message!(Errno::EINVAL, "NV extent entry is not publishing");
            }
        }
        let mut ordered = new_entries.iter().collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|entry| (entry.fs_id(), entry.inode(), entry.logical_start()));
        for pair in ordered.windows(2) {
            if (pair[0].fs_id(), pair[0].inode()) == (pair[1].fs_id(), pair[1].inode())
                && pair[0].logical_range().end > pair[1].logical_start()
            {
                return_errno_with_message!(Errno::EBUSY, "NV extent publication overlaps itself");
            }
        }
        let mut inserted = Vec::new();
        inserted
            .try_reserve_exact(ordered.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for entry in ordered {
            if let Err(error) =
                self.publish_one_entry(entry.clone(), NvExtentEntryState::Publishing)
            {
                for published in inserted.iter().rev() {
                    let _ = self.remove_entry(published);
                }
                return Err(error);
            }
            inserted.push(entry.clone());
        }
        Ok(())
    }

    pub(crate) fn activate_prepared_entries(&self, entries: &[Arc<DirtyNvExtentEntry>]) {
        for entry in entries {
            entry.activate_publication();
        }
    }

    pub(crate) fn discard_prepared_entries(
        &self,
        entries: &[Arc<DirtyNvExtentEntry>],
    ) -> Result<()> {
        for entry in entries.iter().rev() {
            self.remove_entry(entry)?;
        }
        Ok(())
    }

    fn publish_one_entry(
        &self,
        entry: Arc<DirtyNvExtentEntry>,
        expected_state: NvExtentEntryState,
    ) -> Result<()> {
        if entry.state() != expected_state {
            return_errno_with_message!(Errno::EINVAL, "NV extent entry state changed");
        }
        let inode_map = self
            .inode_map(entry.fs_id(), entry.inode(), true)?
            .expect("requested creation returns an inode redirect map");
        let mut entries = inode_map.entries.lock();
        let range = entry.logical_range();
        if Self::entries_overlap(&entries, range.clone()) {
            return_errno_with_message!(Errno::EBUSY, "NV extent entry overlaps an existing entry");
        }
        entries.insert(range.start, entry);
        Ok(())
    }

    pub(crate) fn lookup_dirty_entry(&self, key: PageKey) -> Option<Arc<DirtyNvExtentEntry>> {
        self.lookup_page(key)
    }

    pub(crate) fn lookup_page(&self, key: PageKey) -> Option<Arc<DirtyNvExtentEntry>> {
        let inode_map = self.inode_map(key.fs_id, key.inode, false).ok().flatten()?;
        let entries = inode_map.entries.lock();
        let (_, entry) = entries.range(..=key.page_index).next_back()?;
        entry
            .logical_range()
            .contains(&key.page_index)
            .then(|| entry.clone())
            .filter(|entry| {
                !matches!(
                    entry.state(),
                    NvExtentEntryState::Publishing | NvExtentEntryState::Reclaimed
                )
            })
    }

    pub(crate) fn lookup_overlaps(
        &self,
        fs_id: u64,
        inode: u64,
        range: Range<u64>,
    ) -> Result<Vec<Arc<DirtyNvExtentEntry>>> {
        if range.start >= range.end {
            return_errno_with_message!(Errno::EINVAL, "empty redirect lookup range");
        }
        #[cfg(ktest)]
        self.overlap_lookups.fetch_add(1, Ordering::Relaxed);
        let Some(inode_map) = self.inode_map(fs_id, inode, false)? else {
            return Ok(Vec::new());
        };
        let entries = inode_map.entries.lock();
        let mut overlaps = Vec::new();
        if let Some((_, entry)) = entries.range(..=range.start).next_back()
            && entry.logical_range().end > range.start
        {
            overlaps.push(entry.clone());
        }
        for (_, entry) in entries.range(range.start..) {
            if entry.logical_start() >= range.end {
                break;
            }
            if overlaps
                .last()
                .is_none_or(|previous| !Arc::ptr_eq(previous, entry))
            {
                overlaps.push(entry.clone());
            }
        }
        Ok(overlaps)
    }

    #[cfg(ktest)]
    pub(crate) fn overlap_lookup_count(&self) -> usize {
        self.overlap_lookups.load(Ordering::Acquire)
    }

    #[cfg(ktest)]
    pub(crate) fn dirty_entry_count(&self, fs_id: u64, inode: u64) -> usize {
        self.inode_map(fs_id, inode, false)
            .ok()
            .flatten()
            .map_or(0, |map| map.entries.lock().len())
    }

    #[cfg(ktest)]
    pub(crate) fn page_handle_count(&self, fs_id: u64, inode: u64) -> usize {
        self.partitions
            .iter()
            .map(|partition| {
                partition
                    .lock()
                    .committed
                    .keys()
                    .filter(|key| (key.fs_id, key.inode) == (fs_id, inode))
                    .count()
            })
            .sum()
    }

    pub(crate) fn claim_ready_entries(
        &self,
        fs_id: u64,
        page_budget: usize,
    ) -> Result<Vec<NvExtentEntryLease>> {
        if page_budget == 0 {
            return Ok(Vec::new());
        }
        let start = self
            .next_candidate_partition
            .fetch_add(1, Ordering::Relaxed)
            % self.partitions.len();
        let mut claimed_pages = 0usize;
        let mut leases = Vec::new();
        for offset in 0..self.partitions.len() {
            let partition = (start + offset) % self.partitions.len();
            let inode_maps = self.inode_maps[partition].lock();
            for ((owner_fs_id, _), inode_map) in inode_maps.iter() {
                if *owner_fs_id != fs_id {
                    continue;
                }
                let entries = inode_map.entries.lock();
                for entry in entries.values() {
                    if entry
                        .transition(NvExtentEntryState::Dirty, NvExtentEntryState::Migrating)
                        .is_err()
                    {
                        continue;
                    }
                    claimed_pages = claimed_pages.saturating_add(usize::from(entry.page_count()));
                    leases.push(NvExtentEntryLease {
                        entry: entry.clone(),
                    });
                    if claimed_pages >= page_budget {
                        return Ok(leases);
                    }
                }
            }
        }
        Ok(leases)
    }

    pub(crate) fn complete_entry(&self, lease: &NvExtentEntryLease) -> Result<()> {
        lease.entry.transition(
            NvExtentEntryState::Migrating,
            NvExtentEntryState::IoComplete,
        )
    }

    pub(crate) fn remove_reclaimed_entry(&self, lease: &NvExtentEntryLease) -> Result<()> {
        let entry = lease.entry();
        if entry.state() != NvExtentEntryState::Reclaiming {
            return_errno_with_message!(Errno::ESTALE, "NV extent entry is not reclaiming");
        }
        self.remove_entry(entry)?;
        Ok(())
    }

    pub(crate) fn remove_entry(&self, entry: &Arc<DirtyNvExtentEntry>) -> Result<()> {
        let inode_map = self
            .inode_map(entry.fs_id(), entry.inode(), false)?
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        let mut entries = inode_map.entries.lock();
        if !entries
            .get(&entry.logical_start())
            .is_some_and(|candidate| Arc::ptr_eq(candidate, entry))
        {
            return_errno_with_message!(Errno::ESTALE, "NV extent entry owner changed");
        }
        entries.remove(&entry.logical_start());
        Ok(())
    }

    fn entries_overlap(
        entries: &BTreeMap<u64, Arc<DirtyNvExtentEntry>>,
        range: Range<u64>,
    ) -> bool {
        entries
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, entry)| entry.logical_range().end > range.start)
    }

    fn inode_map(
        &self,
        fs_id: u64,
        inode: u64,
        create: bool,
    ) -> Result<Option<Arc<InodeRedirectMap>>> {
        let partition = inode_redirect_partition_index(fs_id, inode);
        let mut maps = self.inode_maps[partition].lock();
        if let Some(map) = maps.get(&(fs_id, inode)) {
            return Ok(Some(map.clone()));
        }
        if !create {
            return Ok(None);
        }
        let map = Arc::new(InodeRedirectMap::new());
        maps.insert((fs_id, inode), map.clone());
        Ok(Some(map))
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
                Self::refresh_ready_key(state, *key);
                state.epoch = state.epoch.wrapping_add(1);
            }
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
        Self::refresh_ready_key(&mut state, key);
        state.epoch = state.epoch.wrapping_add(1);
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
        Self::refresh_ready_key(&mut state, key);
        state.epoch = state.epoch.wrapping_add(1);
        true
    }

    pub(crate) fn compare_remove(&self, key: PageKey, expected: DescriptorHandle) -> bool {
        self.compare_remove_batch(&[(key, expected)]) == 1
    }

    pub(crate) fn compare_remove_batch(&self, expected: &[(PageKey, DescriptorHandle)]) -> usize {
        let keys = expected.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        let partition_ids = self.partitions_for(&keys);
        let mut guards = partition_ids
            .iter()
            .map(|partition| (*partition, self.partitions[*partition].lock()))
            .collect::<Vec<_>>();
        let mut removed = 0;
        for (key, handle) in expected {
            let partition = self.partition_index(*key);
            let state = guards
                .iter_mut()
                .find(|(index, _)| *index == partition)
                .map(|(_, state)| state)
                .expect("requested redirect partition is locked");
            if state.committed.get(key) != Some(handle) {
                continue;
            }
            state.committed.remove(key);
            state.dirty.remove(key);
            state.ready_set.remove(key);
            state.epoch = state.epoch.wrapping_add(1);
            removed += 1;
        }
        removed
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
            state.ready_pages.clear();
            state.ready_set.clear();
            state.claimed_extents.clear();
            state.epoch = state.epoch.wrapping_add(1);
        }
        for (key, handle) in rebuilt {
            let state = &mut guards[self.partition_index(key)];
            state.committed.insert(key, handle);
            Self::set_dirty(state, key, handle.state);
        }
        for (partition, state) in guards.iter_mut().enumerate() {
            self.rebuild_partition_ready_pages(state, partition)?;
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
            let queued = state.ready_pages.len();
            for _ in 0..queued {
                let Some(seed) = state.ready_pages.pop_front() else {
                    break;
                };
                if !state.ready_set.remove(&seed) {
                    continue;
                }
                self.candidate_extent_visits.fetch_add(1, Ordering::Relaxed);
                let Some(extent) = Self::extent_from_seed(&state, partition, seed)? else {
                    continue;
                };
                if extent.fs_id() != fs_id || extent.len() > remaining {
                    Self::enqueue_ready_key(&mut state, seed);
                    continue;
                }
                state.next_extent_generation = state
                    .next_extent_generation
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                let indexed = IndexedDirtyExtent {
                    extent: extent.clone(),
                    generation: state.next_extent_generation,
                };
                for offset in 0..extent.len() {
                    state.ready_set.remove(&PageKey::new(
                        extent.fs_id(),
                        extent.inode(),
                        extent.logical_start() + offset as u64,
                    ));
                }
                let start_key = extent.start_key();
                remaining -= extent.len();
                state.claimed_extents.insert(start_key, indexed.clone());
                leases.push(MigrationExtentLease {
                    extent,
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
        state.claimed_extents.remove(&start_key);
        for offset in 0..lease.extent.len() {
            Self::refresh_ready_key(
                &mut state,
                PageKey::new(
                    lease.extent.fs_id(),
                    lease.extent.inode(),
                    lease.extent.logical_start() + offset as u64,
                ),
            );
        }
        Ok(())
    }

    pub(crate) fn complete_extent(&self, lease: &MigrationExtentLease) -> Result<()> {
        let partition = lease.extent.partition;
        let start_key = lease.extent.start_key();
        let mut state = self.partitions[partition].lock();
        let Some(indexed) = state.claimed_extents.get(&start_key) else {
            return_errno!(Errno::ESTALE);
        };
        if indexed.generation != lease.generation || indexed.extent != lease.extent {
            return_errno!(Errno::ESTALE);
        }
        state.claimed_extents.remove(&start_key);
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
        self.first_claimed_overlap(fs_id, inode, logical_start, pages)
            .is_some()
    }

    pub(crate) fn first_claimed_overlap(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Option<MigrationConflict> {
        let logical_end = logical_start.checked_add(pages as u64)?;
        let mut cursor = logical_start;
        let mut first = None;
        while cursor < logical_end {
            let key = PageKey::new(fs_id, inode, cursor);
            let partition = self.partition_index(key);
            let state = self.partitions[partition].lock();
            for indexed in state.claimed_extents.values() {
                let extent = &indexed.extent;
                let extent_end = extent.logical_start + extent.pages.len() as u64;
                if extent.fs_id == fs_id
                    && extent.inode == inode
                    && extent.logical_start < logical_end
                    && logical_start < extent_end
                {
                    let conflict = MigrationConflict {
                        logical_start: extent.logical_start,
                        logical_end: extent_end,
                        generation: indexed.generation,
                    };
                    if first.is_none_or(|current: MigrationConflict| {
                        conflict.logical_start < current.logical_start
                    }) {
                        first = Some(conflict);
                    }
                }
            }
            drop(state);
            let next_boundary = cursor
                .checked_div(REDIRECT_PARTITION_PAGES)?
                .checked_add(1)?
                .checked_mul(REDIRECT_PARTITION_PAGES)?;
            cursor = next_boundary.min(logical_end);
        }
        first
    }

    #[cfg(ktest)]
    fn candidate_extent_visits_for_test(&self) -> usize {
        self.candidate_extent_visits.load(Ordering::Relaxed)
    }

    #[cfg(ktest)]
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

    fn key_is_claimed(state: &RedirectPartition, key: PageKey) -> bool {
        state
            .claimed_extents
            .values()
            .any(|indexed| indexed.extent.contains(key))
    }

    fn enqueue_ready_key(state: &mut RedirectPartition, key: PageKey) {
        if state.ready_set.insert(key) {
            state.ready_pages.push_back(key);
        }
    }

    fn refresh_ready_key(state: &mut RedirectPartition, key: PageKey) {
        if state.dirty.contains(&key) && !Self::key_is_claimed(state, key) {
            Self::enqueue_ready_key(state, key);
        } else {
            state.ready_set.remove(&key);
        }
    }

    fn extent_from_seed(
        state: &RedirectPartition,
        partition: usize,
        seed: PageKey,
    ) -> Result<Option<DirtyMigrationExtent>> {
        if !state.dirty.contains(&seed) || Self::key_is_claimed(state, seed) {
            return Ok(None);
        }
        let Some(first) = state.committed.get(&seed).copied() else {
            return Ok(None);
        };
        if first.state != NvObjectState::NvDirty {
            return Ok(None);
        }
        let mut component_start = seed;
        while let Some(page_index) = component_start.page_index.checked_sub(1) {
            let previous_key = PageKey::new(seed.fs_id, seed.inode, page_index);
            if redirect_partition_index(previous_key) != partition
                || !state.dirty.contains(&previous_key)
                || Self::key_is_claimed(state, previous_key)
            {
                break;
            }
            let Some(previous_handle) = state.committed.get(&previous_key).copied() else {
                break;
            };
            let current_handle = state
                .committed
                .get(&component_start)
                .copied()
                .expect("dirty extent component has committed redirects");
            if !dirty_entries_are_contiguous(
                (previous_key, previous_handle),
                (component_start, current_handle),
            ) {
                break;
            }
            component_start = previous_key;
        }
        if !(seed.page_index - component_start.page_index)
            .is_multiple_of(MAX_DIRTY_MIGRATION_EXTENT_PAGES as u64)
        {
            return Ok(None);
        }
        let mut entries = vec![(seed, first)];
        while entries.len() < MAX_DIRTY_MIGRATION_EXTENT_PAGES {
            let previous = *entries.last().expect("migration extent has a seed");
            let Some(page_index) = previous.0.page_index.checked_add(1) else {
                break;
            };
            let key = PageKey::new(seed.fs_id, seed.inode, page_index);
            if redirect_partition_index(key) != partition
                || !state.dirty.contains(&key)
                || Self::key_is_claimed(state, key)
            {
                break;
            }
            let Some(handle) = state.committed.get(&key).copied() else {
                break;
            };
            if !dirty_entries_are_contiguous(previous, (key, handle)) {
                break;
            }
            entries.push((key, handle));
        }
        Ok(Some(DirtyMigrationExtent::try_new(partition, entries)?))
    }

    fn rebuild_partition_ready_pages(
        &self,
        state: &mut RedirectPartition,
        partition: usize,
    ) -> Result<()> {
        #[cfg(ktest)]
        self.dirty_extent_rebuilds.fetch_add(1, Ordering::Relaxed);
        state.ready_pages.clear();
        state.ready_set.clear();
        let dirty = state.dirty.iter().copied().collect::<Vec<_>>();
        for key in dirty {
            if redirect_partition_index(key) != partition {
                return_errno_with_message!(Errno::EUCLEAN, "dirty extent partition mismatch");
            }
            Self::refresh_ready_key(state, key);
        }
        Ok(())
    }

    #[cfg(ktest)]
    fn reset_dirty_extent_rebuilds_for_test(&self) {
        self.dirty_extent_rebuilds.store(0, Ordering::Relaxed);
    }

    #[cfg(ktest)]
    fn dirty_extent_rebuilds_for_test(&self) -> usize {
        self.dirty_extent_rebuilds.load(Ordering::Relaxed)
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
    fn dirty_entry(
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        page_count: u16,
        nv_slot_start: u64,
        ssd_block_start: u64,
    ) -> Arc<DirtyNvExtentEntry> {
        Arc::new(
            DirtyNvExtentEntry::new(
                NvRedirectExtentRecord::new(
                    fs_id,
                    inode,
                    logical_start,
                    page_count,
                    nv_slot_start,
                    ssd_block_start,
                    NvRedirectAuthority::NvAuthoritative,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    }

    fn dirty_nv_entry_preserves_persistent_entry_boundaries() {
        let directory = RedirectDirectory::new().unwrap();
        let first = dirty_entry(0x91, 7, 0, 4, 100, 1000);
        let second = dirty_entry(0x91, 7, 4, 4, 104, 1004);
        directory.publish_dirty_entry(first.clone()).unwrap();
        directory.publish_dirty_entry(second.clone()).unwrap();

        let claimed = directory.claim_ready_entries(0x91, 8).unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(Arc::ptr_eq(claimed[0].entry(), &first));
        assert!(Arc::ptr_eq(claimed[1].entry(), &second));
    }

    fn dirty_nv_entry_crosses_redirect_partition_without_split() {
        let directory = RedirectDirectory::new().unwrap();
        let entry = dirty_entry(0x92, 8, REDIRECT_PARTITION_PAGES - 1, 2, 200, 2000);
        directory.publish_dirty_entry(entry.clone()).unwrap();

        assert!(Arc::ptr_eq(
            &directory
                .lookup_dirty_entry(PageKey::new(0x92, 8, REDIRECT_PARTITION_PAGES))
                .unwrap(),
            &entry
        ));
        let claimed = directory.claim_ready_entries(0x92, 1).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].entry().page_count(), 2);
    }

    fn dirty_nv_entry_budget_never_splits_entry() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .publish_dirty_entry(dirty_entry(0x93, 9, 0, 32, 300, 3000))
            .unwrap();

        let claimed = directory.claim_ready_entries(0x93, 8).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].entry().logical_range(), 0..32);
    }

    fn adjacent_nv_entries_remain_distinct() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .publish_dirty_entry(dirty_entry(0x94, 10, 0, 2, 400, 4000))
            .unwrap();
        directory
            .publish_dirty_entry(dirty_entry(0x94, 10, 2, 2, 402, 4002))
            .unwrap();

        let claimed = directory.claim_ready_entries(0x94, 4).unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].entry().logical_range(), 0..2);
        assert_eq!(claimed[1].entry().logical_range(), 2..4);
    }

    fn dirty_nv_entry_splits_at_ssd_discontinuity() {
        let directory = RedirectDirectory::new().unwrap();
        directory
            .publish_dirty_entry(dirty_entry(0x95, 11, 0, 2, 500, 5000))
            .unwrap();
        directory
            .publish_dirty_entry(dirty_entry(0x95, 11, 2, 2, 502, 9000))
            .unwrap();

        let claimed = directory.claim_ready_entries(0x95, 4).unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].entry().ssd_block_range(), 5000..5002);
        assert_eq!(claimed[1].entry().ssd_block_range(), 9000..9002);
    }

    fn dirty_nv_entry_rejects_more_than_128_pages() {
        assert_eq!(
            NvRedirectExtentRecord::new(
                1,
                2,
                0,
                129,
                100,
                1000,
                NvRedirectAuthority::NvAuthoritative,
            )
            .unwrap_err()
            .error(),
            Errno::E2BIG
        );
    }

    fn nv_redirect_extent_record_has_fixed_layout() {
        assert_eq!(size_of::<NvRedirectExtentRecord>(), 64);
        let record =
            NvRedirectExtentRecord::new(1, 2, 3, 2, 10, 20, NvRedirectAuthority::NvAuthoritative)
                .unwrap();
        assert_eq!(record.logical_range().unwrap(), 3..5);
        assert_eq!(record.nv_slot_range().unwrap(), 10..12);
        assert_eq!(record.ssd_block_range().unwrap(), 20..22);
    }

    #[ktest]
    fn rnvpce_dirty_nv_entry_index() {
        dirty_nv_entry_preserves_persistent_entry_boundaries();
        dirty_nv_entry_crosses_redirect_partition_without_split();
        dirty_nv_entry_budget_never_splits_entry();
        adjacent_nv_entries_remain_distinct();
        dirty_nv_entry_splits_at_ssd_discontinuity();
        dirty_nv_entry_rejects_more_than_128_pages();
        nv_redirect_extent_record_has_fixed_layout();
    }

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

    fn dirty_extent_batch_remove_does_not_rebuild_partition() {
        let directory = RedirectDirectory::new().unwrap();
        let entries = extent_entries(0x67, 8, 0, 2100, 32);
        directory.rebuild(entries.clone()).unwrap();
        directory.reset_dirty_extent_rebuilds_for_test();

        assert_eq!(directory.compare_remove_batch(&entries), entries.len());
        assert_eq!(directory.dirty_extent_rebuilds_for_test(), 0);
        assert!(directory.claim_ready_extents(0x67, 128).unwrap().is_empty());
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
        let conflict = directory
            .first_claimed_overlap(0x88, 10, 0, 4)
            .expect("claimed extent overlaps its exact range");
        assert_eq!((conflict.logical_start, conflict.logical_end), (0, 4));
        assert_eq!(conflict.generation, lease.generation());
        assert!(directory.first_claimed_overlap(0x88, 10, 4, 4).is_none());
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
        dirty_extent_batch_remove_does_not_rebuild_partition();
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
