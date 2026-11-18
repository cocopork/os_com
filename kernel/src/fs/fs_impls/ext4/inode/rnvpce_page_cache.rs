// SPDX-License-Identifier: MPL-2.0

//! Volatile per-inode hot-page index for rNVPCe.

use core::ops::Range;

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
    waiters: WaitQueue,
}

impl RnvpcePageSlot {
    fn loading() -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::Loading,
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn clean_dram(page: CachePage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::CleanDram(page),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn dirty_nv(page: RnvpceNvPage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::DirtyNv(page),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn clean_nv(page: RnvpceNvPage) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::CleanNv(page),
            waiters: WaitQueue::new(),
        })
    }

    pub(crate) fn backing(&self) -> &RnvpcePageBacking {
        &self.backing
    }

    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    #[cfg(ktest)]
    fn new_test(value: u64) -> Arc<Self> {
        Arc::new(Self {
            backing: RnvpcePageBacking::Test(value),
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
}

impl RnvpcePageCache {
    pub(crate) const fn new() -> Self {
        Self {
            pages: XArray::new(),
        }
    }

    pub(crate) fn lookup(&self, page_index: u64) -> Option<Arc<RnvpcePageSlot>> {
        let guard = disable_preempt();
        self.pages
            .load(&guard, page_index)
            .map(|slot| Arc::clone(&slot))
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
        }
        for old in replaced {
            old.waiters.wake_all();
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ostd::prelude::ktest;

    use super::{RnvpcePageBacking, RnvpcePageCache, RnvpcePageSlot};
    use crate::{prelude::*, thread::kernel_thread::ThreadOptions};

    fn test_slot(value: u64) -> Arc<RnvpcePageSlot> {
        RnvpcePageSlot::new_test(value)
    }

    #[ktest]
    fn rnvpce_page_cache_one_loader_owns_an_absent_page() {
        let cache = RnvpcePageCache::new();
        let first = cache.get_or_reserve_loading(7).unwrap();
        let second = cache.get_or_reserve_loading(7).unwrap();

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
