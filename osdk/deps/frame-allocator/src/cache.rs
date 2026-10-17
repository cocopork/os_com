// SPDX-License-Identifier: MPL-2.0

//! A fixed-size local cache for frame allocation.

use core::{alloc::Layout, cell::RefCell};

use ostd::{
    cpu_local,
    irq::DisabledLocalIrqGuard,
    mm::{PAGE_SIZE, Paddr},
};

cpu_local! {
    static CACHE: RefCell<CacheOfSizes> = RefCell::new(CacheOfSizes::new());
}

struct CacheOfSizes {
    cache1: CacheArray<1, 12>,
    cache2: CacheArray<2, 6>,
    cache3: CacheArray<3, 6>,
    cache4: CacheArray<4, 6>,
}

/// A fixed-size local cache for frame allocation.
///
/// Each cache array contains at most `COUNT` segments. Each segment contains
/// `NR_CONT_FRAMES` contiguous frames.
struct CacheArray<const NR_CONT_FRAMES: usize, const COUNT: usize> {
    inner: [Option<Paddr>; COUNT],
    size: usize,
}

impl<const NR_CONT_FRAMES: usize, const COUNT: usize> CacheArray<NR_CONT_FRAMES, COUNT> {
    const fn new() -> Self {
        Self {
            inner: [const { None }; COUNT],
            size: 0,
        }
    }

    /// The size of the segments that this cache manages.
    const fn segment_size() -> usize {
        NR_CONT_FRAMES * PAGE_SIZE
    }

    /// Allocates a segment of frames.
    ///
    /// It may allocate directly from this cache. If the cache is empty, it
    /// will fill the cache.
    fn alloc(&mut self, guard: &DisabledLocalIrqGuard) -> Option<Paddr> {
        self.alloc_with(|layout| super::pools::alloc(guard, layout))
    }

    fn alloc_with(&mut self, mut allocate: impl FnMut(Layout) -> Option<Paddr>) -> Option<Paddr> {
        if let Some(frame) = self.pop_front() {
            return Some(frame);
        }

        let nr_to_alloc = COUNT * 2 / 3;
        let Some(allocated) = allocate(
            Layout::from_size_align(nr_to_alloc * Self::segment_size(), PAGE_SIZE).unwrap(),
        ) else {
            // Refilling is an optimization, not part of the caller's request.
            // Fragmented pools can satisfy one segment without containing a
            // contiguous batch. Do not report OOM until that request fails too.
            return allocate(Layout::from_size_align(Self::segment_size(), PAGE_SIZE).unwrap());
        };

        for i in 1..nr_to_alloc {
            self.push_front(allocated + i * Self::segment_size());
        }

        Some(allocated)
    }

    /// Deallocates a segment of frames.
    ///
    /// It may deallocate directly to this cache. If the cache is full, it will
    /// deallocate to the global pool.
    fn dealloc(&mut self, guard: &DisabledLocalIrqGuard, addr: Paddr) {
        if self.push_front(addr).is_none() {
            let nr_to_dealloc = COUNT * 2 / 3 + 1;

            let segments = (0..nr_to_dealloc).map(|i| {
                if i == 0 {
                    (addr, Self::segment_size())
                } else {
                    (self.pop_front().unwrap(), Self::segment_size())
                }
            });

            super::pools::dealloc(guard, segments);
        };
    }

    fn push_front(&mut self, frame: Paddr) -> Option<()> {
        if self.size == COUNT {
            return None;
        }

        self.inner[self.size] = Some(frame);
        self.size += 1;
        Some(())
    }

    fn pop_front(&mut self) -> Option<Paddr> {
        if self.size == 0 {
            return None;
        }

        let frame = self.inner[self.size - 1].take().unwrap();
        self.size -= 1;
        Some(frame)
    }
}

impl CacheOfSizes {
    const fn new() -> Self {
        Self {
            cache1: CacheArray::new(),
            cache2: CacheArray::new(),
            cache3: CacheArray::new(),
            cache4: CacheArray::new(),
        }
    }
}

pub(super) fn alloc(guard: &DisabledLocalIrqGuard, layout: Layout) -> Option<Paddr> {
    let nr_frames = layout.size() / PAGE_SIZE;
    if layout.align() > layout.size() {
        return super::pools::alloc(guard, layout);
    }

    let cache_cell = CACHE.get_with(guard);
    let mut cache = cache_cell.borrow_mut();

    match nr_frames {
        1 => cache.cache1.alloc(guard),
        2 => cache.cache2.alloc(guard),
        3 => cache.cache3.alloc(guard),
        4 => cache.cache4.alloc(guard),
        _ => super::pools::alloc(guard, layout),
    }
}

pub(super) fn dealloc(guard: &DisabledLocalIrqGuard, addr: Paddr, size: usize) {
    let nr_frames = size / PAGE_SIZE;
    if nr_frames > 4 {
        super::pools::dealloc(guard, [(addr, size)].into_iter());
        return;
    }

    let cache_cell = CACHE.get_with(guard);
    let mut cache = cache_cell.borrow_mut();

    match nr_frames {
        1 => cache.cache1.dealloc(guard, addr),
        2 => cache.cache2.dealloc(guard, addr),
        3 => cache.cache3.dealloc(guard, addr),
        4 => cache.cache4.dealloc(guard, addr),
        _ => super::pools::dealloc(guard, [(addr, size)].into_iter()),
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        chunk::{greater_order_of, split_to_chunks},
        set::BuddySet,
        test::MockMemoryRegion,
    };

    fn check_refill_fallback<const NR: usize, const COUNT: usize>() {
        let region = MockMemoryRegion::alloc(16 * PAGE_SIZE);
        let start = region.paddr();
        let mut pool = BuddySet::<5>::new_empty();
        split_to_chunks(start, NR.next_power_of_two() * PAGE_SIZE)
            .for_each(|(addr, order)| pool.insert_chunk(addr, order));
        let mut cache = CacheArray::<NR, COUNT>::new();
        let allocated = cache.alloc_with(|layout| {
            let order = greater_order_of(layout.size());
            let addr = pool.alloc_chunk(order)?;
            let remainder = (PAGE_SIZE << order) - layout.size();
            split_to_chunks(addr + layout.size(), remainder)
                .for_each(|(addr, order)| pool.insert_chunk(addr, order));
            Some(addr)
        });
        let cached = cache.size;
        let free = pool.total_size();
        // Drain the mock region before RED assertions as well.
        while cache.pop_front().is_some() {}
        while pool.alloc_chunk(0).is_some() {}
        assert_eq!(
            allocated,
            Some(start),
            "cache refill must not turn an available small request into OOM"
        );
        assert_eq!(cached, 0);
        assert_eq!(free, (NR.next_power_of_two() - NR) * PAGE_SIZE);
    }

    #[ktest]
    fn small_request_survives_failed_cache_refill() {
        check_refill_fallback::<1, 12>();
        check_refill_fallback::<2, 6>();
        check_refill_fallback::<3, 6>();
        check_refill_fallback::<4, 6>();
    }

    #[ktest]
    fn empty_cache_and_pool_report_no_memory() {
        let mut cache = CacheArray::<1, 12>::new();
        assert!(cache.alloc_with(|_| None).is_none());
        assert_eq!(cache.size, 0);
    }
}
