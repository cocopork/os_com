// SPDX-License-Identifier: MPL-2.0

//! Volatile per-inode hot-page index for rNVPCe.

use core::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use ostd::{sync::WaitQueue, task::disable_preempt};
use xarray::XArray;

use crate::{fs::rnvpc::redirect_index::DirtyNvExtentEntry, prelude::*, vm::page_cache::CachePage};

/// The persistent NV range and logical page selected by one cache slot.
pub(crate) struct RnvpceNvPage {
    entry: Arc<DirtyNvExtentEntry>,
    logical_page: u64,
}

impl RnvpceNvPage {
    pub(crate) fn new(entry: Arc<DirtyNvExtentEntry>, logical_page: u64) -> Self {
        Self {
            entry,
            logical_page,
        }
    }

    pub(crate) fn entry(&self) -> &Arc<DirtyNvExtentEntry> {
        &self.entry
    }

    pub(crate) const fn logical_page(&self) -> u64 {
        self.logical_page
    }
}

/// Immutable backing selected by a published page slot.
pub(crate) enum RnvpcePageBacking {
    Loading,
    CleanDram(CachePage),
    DirtyNv(RnvpceNvPage),
    CleanNv(RnvpceNvPage),
    #[cfg(ktest)]
    Test(u64),
}

/// A reference-counted publication object. Waiting is page-local.
pub(crate) struct RnvpcePageSlot {
    backing: RnvpcePageBacking,
    accessed: AtomicBool,
    waiters: WaitQueue,
}

impl RnvpcePageSlot {
    fn loading() -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::Loading,
            accessed: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn clean_dram(page: CachePage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::CleanDram(page),
            accessed: AtomicBool::new(true),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn dirty_nv(page: RnvpceNvPage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::DirtyNv(page),
            accessed: AtomicBool::new(true),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn clean_nv(page: RnvpceNvPage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::CleanNv(page),
            accessed: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn backing(&self) -> &RnvpcePageBacking {
        &self.backing
    }

    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    pub(crate) fn mark_accessed(&self) {
        self.accessed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take_accessed(&self) -> bool {
        self.accessed.swap(false, Ordering::AcqRel)
    }

    #[cfg(ktest)]
    fn new_test(value: u64) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::Test(value),
            accessed: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        })
    }
}

pub(crate) struct LoadingPage {
    slot: Arc<RnvpcePageSlot>,
    owner: bool,
}

impl LoadingPage {
    pub(crate) fn slot(&self) -> &Arc<RnvpcePageSlot> {
        &self.slot
    }

    pub(crate) const fn is_owner(&self) -> bool {
        self.owner
    }
}

/// One instance belongs to one inode. Reads are RCU-based; the XArray writer
/// lock is held only while publishing or removing an `Arc`.
pub(crate) struct RnvpcePageCache {
    pages: XArray<Arc<RnvpcePageSlot>>,
    ever_populated: AtomicBool,
}

impl RnvpcePageCache {
    pub(crate) const fn new() -> Self {
        Self {
            pages: XArray::new(),
            ever_populated: AtomicBool::new(false),
        }
    }

    /// False is an exact fast path; true only means that invalidation may be
    /// necessary because this inode has populated the hot-page index before.
    pub(crate) fn may_contain_pages(&self) -> bool {
        self.ever_populated.load(Ordering::Acquire)
    }

    pub(crate) fn lookup(&self, page_index: u64) -> Option<Arc<RnvpcePageSlot>> {
        let guard = disable_preempt();
        let slot = self
            .pages
            .load(&guard, page_index)
            .map(|slot| Arc::clone(&slot));
        if let Some(slot) = &slot {
            slot.mark_accessed();
        }
        slot
    }

    pub(crate) fn get_or_reserve_loading(&self, page_index: u64) -> Result<LoadingPage> {
        let mut pages = self.pages.lock();
        if let Some(slot) = pages.load(page_index) {
            return Ok(LoadingPage {
                slot: Arc::clone(&slot),
                owner: false,
            });
        }
        let slot = RnvpcePageSlot::loading();
        pages.store(page_index, slot.clone());
        self.ever_populated.store(true, Ordering::Release);
        Ok(LoadingPage { slot, owner: true })
    }

    /// Returns a resident slot or runs the one page-local loader outside all
    /// XArray locks. Concurrent callers sleep on this page's wait queue.
    pub(crate) fn get_or_load<F>(&self, page_index: u64, load: F) -> Result<Arc<RnvpcePageSlot>>
    where
        F: FnOnce() -> Result<Arc<RnvpcePageSlot>>,
    {
        let reservation = self.get_or_reserve_loading(page_index)?;
        if !reservation.is_owner() {
            if !matches!(reservation.slot().backing(), RnvpcePageBacking::Loading) {
                return Ok(reservation.slot().clone());
            }
            return reservation
                .slot()
                .waiters
                .wait_until(|| match self.lookup(page_index) {
                    Some(current) if !Arc::ptr_eq(&current, reservation.slot()) => {
                        Some(Ok(current))
                    }
                    None => Some(Err(Error::new(Errno::ESTALE))),
                    _ => None,
                });
        }

        match load() {
            Ok(loaded) => {
                self.replace_exact(page_index, reservation.slot(), loaded.clone())?;
                Ok(loaded)
            }
            Err(error) => {
                let _ = self.remove_exact(page_index, reservation.slot());
                Err(error)
            }
        }
    }

    pub(crate) fn wait_for_replacement(
        &self,
        page_index: u64,
        loading: &Arc<RnvpcePageSlot>,
    ) -> Result<Arc<RnvpcePageSlot>> {
        loading
            .waiters
            .wait_until(|| match self.lookup(page_index) {
                Some(current) if !Arc::ptr_eq(&current, loading) => Some(Ok(current)),
                None => Some(Err(Error::new(Errno::ESTALE))),
                _ => None,
            })
    }

    pub(crate) fn replace_exact(
        &self,
        page_index: u64,
        expected: &Arc<RnvpcePageSlot>,
        replacement: Arc<RnvpcePageSlot>,
    ) -> Result<()> {
        let mut pages = self.pages.lock();
        let Some(current) = pages.load(page_index) else {
            return_errno_with_message!(Errno::ESTALE, "rNVPCe page slot disappeared");
        };
        if !Arc::ptr_eq(&current, expected) {
            return_errno_with_message!(Errno::ESTALE, "rNVPCe page slot changed");
        }
        pages.store(page_index, replacement);
        drop(pages);
        expected.waiters.wake_all();
        Ok(())
    }

    pub(crate) fn remove_exact(
        &self,
        page_index: u64,
        expected: &Arc<RnvpcePageSlot>,
    ) -> Result<Arc<RnvpcePageSlot>> {
        let mut pages = self.pages.lock();
        let Some(current) = pages.load(page_index) else {
            return_errno_with_message!(Errno::ESTALE, "rNVPCe page slot disappeared");
        };
        if !Arc::ptr_eq(&current, expected) {
            return_errno_with_message!(Errno::ESTALE, "rNVPCe page slot changed");
        }
        let removed = pages
            .remove(page_index)
            .map(|slot| Arc::clone(&slot))
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        drop(pages);
        expected.waiters.wake_all();
        Ok(removed)
    }

    pub(crate) fn snapshot_range(
        &self,
        range: Range<u64>,
        limit: usize,
    ) -> Vec<(u64, Arc<RnvpcePageSlot>)> {
        let guard = disable_preempt();
        self.pages
            .range(&guard, range)
            .take(limit)
            .map(|(index, slot)| (index, Arc::clone(&slot)))
            .collect()
    }

    /// Removes a bounded logical range with one per-inode writer acquisition.
    /// Wakeups happen after dropping the XArray lock.
    pub(crate) fn invalidate_range(&self, range: Range<u64>) {
        let mut removed = Vec::new();
        {
            let mut pages = self.pages.lock();
            for index in range {
                if let Some(slot) = pages.remove(index) {
                    removed.push(Arc::clone(&slot));
                }
            }
        }
        for slot in removed {
            slot.waiters.wake_all();
        }
    }

    /// Builds an entire replacement outside the XArray lock, then publishes
    /// it with one per-inode writer acquisition.
    pub(crate) fn replace_range_with<F>(&self, range: Range<u64>, mut build: F) -> Result<()>
    where
        F: FnMut(u64) -> Result<Arc<RnvpcePageSlot>>,
    {
        let count = usize::try_from(range.end.saturating_sub(range.start))
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let mut replacements = Vec::new();
        replacements
            .try_reserve_exact(count)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for index in range {
            replacements.push((index, build(index)?));
        }

        let mut replaced = Vec::new();
        replaced
            .try_reserve_exact(replacements.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        {
            let mut pages = self.pages.lock();
            for (index, replacement) in replacements {
                if let Some(old) = pages.load(index) {
                    replaced.push(Arc::clone(&old));
                }
                pages.store(index, replacement);
            }
            self.ever_populated.store(true, Ordering::Release);
        }
        for old in replaced {
            old.waiters.wake_all();
        }
        Ok(())
    }

    /// Converts only slots that still name `entry`. Rewritten pages and newer
    /// publications are left untouched. Replacement objects are allocated
    /// before the single per-inode XArray writer critical section.
    pub(crate) fn publish_clean_nv(&self, entry: &Arc<DirtyNvExtentEntry>) -> usize {
        let mut replacements = Vec::new();
        for (index, slot) in self.snapshot_range(entry.logical_range(), usize::MAX) {
            let RnvpcePageBacking::DirtyNv(page) = slot.backing() else {
                continue;
            };
            if !Arc::ptr_eq(page.entry(), entry) {
                continue;
            }
            let replacement = RnvpcePageSlot::clean_nv(RnvpceNvPage::new(entry.clone(), index));
            replacements.push((index, slot, replacement));
        }

        let mut replaced = Vec::new();
        {
            let mut pages = self.pages.lock();
            for (index, expected, replacement) in replacements {
                let Some(current) = pages.load(index) else {
                    continue;
                };
                if !Arc::ptr_eq(&current, &expected) {
                    continue;
                }
                pages.store(index, replacement);
                replaced.push(expected);
            }
        }
        let count = replaced.len();
        for old in replaced {
            old.waiters.wake_all();
        }
        count
    }

    pub(crate) fn clean_nv_candidates(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
    ) -> Vec<(u64, Arc<RnvpcePageSlot>, bool)> {
        self.snapshot_range(entry.logical_range(), usize::MAX)
            .into_iter()
            .filter_map(|(index, slot)| {
                let RnvpcePageBacking::CleanNv(page) = slot.backing() else {
                    return None;
                };
                Arc::ptr_eq(page.entry(), entry)
                    .then(|| (index, slot.clone(), slot.take_accessed()))
            })
            .collect()
    }

    /// Applies a prepared clean-NV retirement plan with one XArray writer
    /// acquisition. `None` evicts a cold page; `Some` installs a DRAM copy.
    pub(crate) fn retire_clean_nv(
        &self,
        operations: Vec<(u64, Arc<RnvpcePageSlot>, Option<Arc<RnvpcePageSlot>>)>,
    ) -> usize {
        let mut retired = Vec::new();
        {
            let mut pages = self.pages.lock();
            for (index, expected, replacement) in operations {
                let Some(current) = pages.load(index) else {
                    continue;
                };
                if !Arc::ptr_eq(&current, &expected) {
                    continue;
                }
                if let Some(replacement) = replacement {
                    pages.store(index, replacement);
                } else {
                    pages.remove(index);
                }
                retired.push(expected);
            }
        }
        let count = retired.len();
        for old in retired {
            old.waiters.wake_all();
        }
        count
    }
}

#[cfg(ktest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ostd::prelude::ktest;

    use super::{RnvpceNvPage, RnvpcePageBacking, RnvpcePageCache, RnvpcePageSlot};
    use crate::{
        fs::rnvpc::{
            descriptor::{NvRedirectAuthority, NvRedirectExtentRecord},
            redirect_index::DirtyNvExtentEntry,
        },
        prelude::*,
        thread::kernel_thread::ThreadOptions,
    };

    fn test_slot(value: u64) -> Arc<RnvpcePageSlot> {
        RnvpcePageSlot::new_test(value)
    }

    #[ktest]
    fn rnvpce_page_cache_one_loader_owns_an_absent_page() {
        let cache = RnvpcePageCache::new();
        assert!(!cache.may_contain_pages());
        let first = cache.get_or_reserve_loading(7).unwrap();
        let second = cache.get_or_reserve_loading(7).unwrap();

        assert!(cache.may_contain_pages());
        assert!(first.is_owner());
        assert!(!second.is_owner());
        assert!(Arc::ptr_eq(first.slot(), second.slot()));
        assert!(matches!(first.slot().backing(), RnvpcePageBacking::Loading));
    }

    #[ktest]
    fn rnvpce_page_cache_exact_owner_can_replace_a_page() {
        let cache = RnvpcePageCache::new();
        let loading = cache.get_or_reserve_loading(11).unwrap();
        let resident = test_slot(41);

        cache
            .replace_exact(11, loading.slot(), resident.clone())
            .unwrap();

        let found = cache.lookup(11).unwrap();
        assert!(Arc::ptr_eq(&found, &resident));
    }

    #[ktest]
    fn rnvpce_page_cache_stale_owner_cannot_replace_a_newer_page() {
        let cache = RnvpcePageCache::new();
        let old = cache.get_or_reserve_loading(3).unwrap();
        let current = test_slot(1);
        cache.replace_exact(3, old.slot(), current.clone()).unwrap();

        assert!(cache.replace_exact(3, old.slot(), test_slot(2)).is_err());
        assert!(Arc::ptr_eq(&cache.lookup(3).unwrap(), &current));
    }

    #[ktest]
    fn rnvpce_page_cache_mutations_on_different_pages_do_not_share_ownership() {
        let cache = RnvpcePageCache::new();
        let left = cache.get_or_reserve_loading(1).unwrap();
        let right = cache.get_or_reserve_loading(2).unwrap();

        assert!(left.is_owner());
        assert!(right.is_owner());
        assert!(!Arc::ptr_eq(left.slot(), right.slot()));
        cache.replace_exact(2, right.slot(), test_slot(22)).unwrap();
        assert!(Arc::ptr_eq(cache.lookup(1).as_ref().unwrap(), left.slot()));
    }

    #[ktest]
    fn rnvpce_page_cache_range_snapshot_is_bounded_and_sparse() {
        let cache = RnvpcePageCache::new();
        for index in [1, 4, 9, 12] {
            let loading = cache.get_or_reserve_loading(index).unwrap();
            cache
                .replace_exact(index, loading.slot(), test_slot(index))
                .unwrap();
        }

        let snapshot = cache.snapshot_range(3..10, 2);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].0, 4);
        assert_eq!(snapshot[1].0, 9);
    }

    #[ktest]
    fn rnvpce_page_cache_lookup_marks_only_the_target_page_accessed() {
        let cache = RnvpcePageCache::new();
        let left = cache.get_or_reserve_loading(1).unwrap();
        let left_slot = test_slot(1);
        cache
            .replace_exact(1, left.slot(), left_slot.clone())
            .unwrap();
        let right = cache.get_or_reserve_loading(2).unwrap();
        let right_slot = test_slot(2);
        cache
            .replace_exact(2, right.slot(), right_slot.clone())
            .unwrap();

        cache.lookup(1).unwrap();

        assert!(left_slot.take_accessed());
        assert!(!right_slot.take_accessed());
    }

    #[ktest]
    fn rnvpce_clean_nv_recency_starts_at_publication() {
        let cache = RnvpcePageCache::new();
        let record = NvRedirectExtentRecord::new_with_visibility(
            1,
            2,
            7,
            1,
            11,
            13,
            NvRedirectAuthority::NvAuthoritative,
            1,
        )
        .unwrap();
        let entry = Arc::new(DirtyNvExtentEntry::new(record).unwrap());
        let loading = cache.get_or_reserve_loading(7).unwrap();
        cache
            .replace_exact(
                7,
                loading.slot(),
                RnvpcePageSlot::dirty_nv(RnvpceNvPage::new(entry.clone(), 7)),
            )
            .unwrap();

        assert_eq!(cache.publish_clean_nv(&entry), 1);
        let candidates = cache.clean_nv_candidates(&entry);
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].2, "a write is not a clean-page cache hit");

        cache.lookup(7).unwrap();
        assert!(cache.clean_nv_candidates(&entry)[0].2);
    }

    #[ktest]
    fn rnvpce_page_cache_retires_a_batch_with_exact_owners() {
        let cache = RnvpcePageCache::new();
        let mut operations = Vec::new();
        for index in 4..8 {
            let loading = cache.get_or_reserve_loading(index).unwrap();
            let resident = test_slot(index);
            cache
                .replace_exact(index, loading.slot(), resident.clone())
                .unwrap();
            operations.push((index, resident, None));
        }

        assert_eq!(cache.retire_clean_nv(operations), 4);
        assert!(cache.snapshot_range(4..8, 4).is_empty());
    }

    #[ktest]
    fn rnvpce_page_cache_hit_does_not_call_loader() {
        let cache = RnvpcePageCache::new();
        let loading = cache.get_or_reserve_loading(8).unwrap();
        let resident = test_slot(8);
        cache
            .replace_exact(8, loading.slot(), resident.clone())
            .unwrap();
        let loads = AtomicUsize::new(0);

        let found = cache
            .get_or_load(8, || {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok(test_slot(99))
            })
            .unwrap();

        assert!(Arc::ptr_eq(&found, &resident));
        assert_eq!(loads.load(Ordering::Relaxed), 0);
    }

    #[ktest]
    fn rnvpce_page_cache_concurrent_miss_has_one_loader() {
        let cache = Arc::new(RnvpcePageCache::new());
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let loads = Arc::new(AtomicUsize::new(0));
        let first_result = Arc::new(Mutex::new(None));
        let second_result = Arc::new(Mutex::new(None));
        let first = {
            let cache = cache.clone();
            let started = started.clone();
            let release = release.clone();
            let loads = loads.clone();
            let first_result = first_result.clone();
            ThreadOptions::new(move || {
                let slot = cache
                    .get_or_load(19, || {
                        loads.fetch_add(1, Ordering::Relaxed);
                        started.store(true, Ordering::Release);
                        while !release.load(Ordering::Acquire) {
                            crate::thread::Thread::yield_now();
                        }
                        Ok(test_slot(19))
                    })
                    .unwrap();
                *first_result.lock() = Some(slot);
            })
            .spawn()
        };
        while !started.load(Ordering::Acquire) {
            crate::thread::Thread::yield_now();
        }
        let second = {
            let cache = cache.clone();
            let loads = loads.clone();
            let second_result = second_result.clone();
            ThreadOptions::new(move || {
                let slot = cache
                    .get_or_load(19, || {
                        loads.fetch_add(1, Ordering::Relaxed);
                        Ok(test_slot(119))
                    })
                    .unwrap();
                *second_result.lock() = Some(slot);
            })
            .spawn()
        };
        release.store(true, Ordering::Release);
        first.join();
        second.join();
        let first_slot = first_result.lock().clone().unwrap();
        let second_slot = second_result.lock().clone().unwrap();

        assert!(Arc::ptr_eq(&first_slot, &second_slot));
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[ktest]
    fn rnvpce_page_cache_range_publication_is_all_or_nothing_before_lock() {
        let cache = RnvpcePageCache::new();
        for index in 4..7 {
            let loading = cache.get_or_reserve_loading(index).unwrap();
            cache
                .replace_exact(index, loading.slot(), test_slot(index))
                .unwrap();
        }

        let result = cache.replace_range_with(4..7, |index| {
            if index == 5 {
                return Err(Error::new(Errno::EIO));
            }
            Ok(test_slot(index + 100))
        });

        assert!(result.is_err());
        for index in 4..7 {
            assert!(matches!(
                cache.lookup(index).unwrap().backing(),
                RnvpcePageBacking::Test(value) if *value == index
            ));
        }
    }

    #[ktest]
    fn rnvpce_page_cache_range_publication_replaces_every_page() {
        let cache = RnvpcePageCache::new();
        for index in 20..24 {
            let loading = cache.get_or_reserve_loading(index).unwrap();
            cache
                .replace_exact(index, loading.slot(), test_slot(index))
                .unwrap();
        }

        cache
            .replace_range_with(20..24, |index| Ok(test_slot(index + 100)))
            .unwrap();

        for index in 20..24 {
            assert!(matches!(
                cache.lookup(index).unwrap().backing(),
                RnvpcePageBacking::Test(value) if *value == index + 100
            ));
        }
    }
}
