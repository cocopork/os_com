// SPDX-License-Identifier: MPL-2.0

//! Controlling the balancing between CPU-local free pools and the global free pool.

use ostd::cpu::num_cpus;

use super::{BuddyOrder, BuddySet, MAX_LOCAL_BUDDY_ORDER, OnDemandGlobalLock, lesser_order_of};

use crate::chunk::split_to_order;

/// Controls the expected size of cache for each CPU-local free pool.
///
/// The expected size will be the size of `GLOBAL_POOL` divided by the number
/// of the CPUs, and then divided by this constant.
const CACHE_EXPECTED_PORTION: usize = 2;

/// Returns the expected size of cache for each CPU-local free pool.
///
/// It depends on the size of the global free pool.
fn cache_expected_size(global_size: usize) -> usize {
    global_size / num_cpus() / CACHE_EXPECTED_PORTION
}

/// Controls the minimal size of cache for each CPU-local free pool.
///
/// The minimal will be the expected size divided by this constant.
const CACHE_MINIMAL_PORTION: usize = 8;

/// Returns the minimal size of cache for each CPU-local free pool.
///
/// It depends on the size of the global free pool.
fn cache_minimal_size(global_size: usize) -> usize {
    cache_expected_size(global_size) / CACHE_MINIMAL_PORTION
}

/// Controls the maximal size of cache for each CPU-local free pool.
///
/// The maximal will be the expected size multiplied by this constant.
const CACHE_MAXIMAL_MULTIPLIER: usize = 2;

/// Returns the maximal size of cache for each CPU-local free pool.
///
/// It depends on the size of the global free pool.
fn cache_maximal_size(global_size: usize) -> usize {
    cache_expected_size(global_size) * CACHE_MAXIMAL_MULTIPLIER
}

/// Balances a local cache and the global free pool.
pub fn balance(local: &mut BuddySet<MAX_LOCAL_BUDDY_ORDER>, global: &mut OnDemandGlobalLock) {
    let global_size = global.get_global_size();

    let minimal_local_size = cache_minimal_size(global_size);
    let expected_local_size = cache_expected_size(global_size);
    let maximal_local_size = cache_maximal_size(global_size);

    let local_size = local.total_size();

    if local_size >= maximal_local_size {
        // Move local frames to the global pool.
        if local_size == 0 {
            return;
        }

        let expected_removal = local_size - expected_local_size;
        let lesser_order = lesser_order_of(expected_removal);

        balance_to(local, &mut *global.get(), lesser_order);
    } else if local_size < minimal_local_size {
        // Move global frames to the local pool.
        if global_size == 0 {
            return;
        }

        let expected_allocation = expected_local_size - local_size;
        let lesser_order = lesser_order_of(expected_allocation);

        balance_to(&mut *global.get(), local, lesser_order);
    }
}

/// Balances from `a` to `b`.
fn balance_to<const MAX_ORDER1: BuddyOrder, const MAX_ORDER2: BuddyOrder>(
    a: &mut BuddySet<MAX_ORDER1>,
    b: &mut BuddySet<MAX_ORDER2>,
    order: BuddyOrder,
) {
    // Stop every recursive branch once the source is exhausted. Otherwise an
    // impossible large request explores an exponential tree of empty pools.
    if a.total_size() == 0 {
        return;
    }
    let allocated_from_a = a.alloc_chunk(order);

    if let Some(addr) = allocated_from_a {
        if order >= MAX_ORDER2 {
            let inserted_order = MAX_ORDER2 - 1;
            split_to_order(addr, order, inserted_order).for_each(|addr| {
                b.insert_chunk(addr, inserted_order);
            });
        } else {
            b.insert_chunk(addr, order);
        }
    } else {
        // Maybe the chunk size is too large.
        // Try to reduce the order and balance again.
        // Order zero is a 4 KiB page. Reclaim commonly returns isolated pages
        // that cannot coalesce; they must still move out of an oversized pool.
        if order > 0 {
            balance_to(a, b, order - 1);
            balance_to(a, b, order - 1);
        }
    }
}

/// Returns every free chunk to a common pool so cross-CPU buddies can merge.
/// The caller holds the source pool lock before the destination global lock.
pub(super) fn drain_to<const A: BuddyOrder, const B: BuddyOrder>(
    source: &mut BuddySet<A>,
    target: &mut BuddySet<B>,
) {
    for order in (0..A).rev() {
        while let Some(addr) = source.alloc_chunk(order) {
            if order >= B {
                split_to_order(addr, order, B - 1)
                    .for_each(|addr| target.insert_chunk(addr, B - 1));
            } else {
                target.insert_chunk(addr, order);
            }
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::{mm::PAGE_SIZE, prelude::ktest};

    use super::*;
    use crate::test::MockMemoryRegion;

    fn drain<const MAX: BuddyOrder>(pool: &mut BuddySet<MAX>, start: usize) -> usize {
        let mut seen = 0;
        while let Some(addr) = pool.alloc_chunk(0) {
            let index = (addr - start) / PAGE_SIZE;
            assert_eq!(seen & (1 << index), 0, "a frame was returned twice");
            seen |= 1 << index;
        }
        assert_eq!(pool.total_size(), 0);
        seen
    }

    #[ktest]
    fn balance_scattered_single_pages_preserves_frames() {
        let region = MockMemoryRegion::alloc(8 * PAGE_SIZE);
        let start = region.paddr();
        let mut source = BuddySet::<5>::new_empty();
        let mut target = BuddySet::<5>::new_empty();
        for index in [0, 2, 4, 6] {
            source.insert_chunk(start + index * PAGE_SIZE, 0);
        }
        balance_to(&mut source, &mut target, 1);
        let retained_size = source.total_size();
        let moved_size = target.total_size();
        let moved = drain(&mut target, start);
        let retained = drain(&mut source, start);
        // Clean up the mock region before assertions, including on RED runs.
        assert_eq!(retained_size, 2 * PAGE_SIZE);
        assert_eq!(moved_size, 2 * PAGE_SIZE);
        assert_eq!(moved & retained, 0);
        assert_eq!(moved | retained, 0b0101_0101);
    }

    #[ktest]
    fn balance_exhausted_scattered_pool_stops() {
        let region = MockMemoryRegion::alloc(8 * PAGE_SIZE);
        let start = region.paddr();
        let mut source = BuddySet::<5>::new_empty();
        let mut target = BuddySet::<5>::new_empty();
        for index in [0, 2, 6] {
            source.insert_chunk(start + index * PAGE_SIZE, 0);
        }
        balance_to(&mut source, &mut target, 6);
        let retained_size = source.total_size();
        let moved_size = target.total_size();
        let retained = drain(&mut source, start);
        // An empty pool must not expand a binary search tree for a large
        // request. This is also exercised after exhaustion during recursion.
        if retained_size == 0 {
            balance_to(&mut source, &mut target, 31);
        }
        let moved = drain(&mut target, start);
        assert_eq!(retained_size, 0);
        assert_eq!(retained, 0);
        assert_eq!(moved_size, 3 * PAGE_SIZE);
        assert_eq!(moved, 0b0100_0101);
    }
    #[ktest]
    fn scattered_cpu_pools_coalesce_for_large_request() {
        let region = MockMemoryRegion::alloc(16 * PAGE_SIZE);
        let start = region.paddr();
        let mut first = BuddySet::<5>::new_empty();
        let mut second = BuddySet::<5>::new_empty();
        let mut global = BuddySet::<5>::new_empty();
        for index in 0..16 {
            match index % 3 {
                0 => first.insert_chunk(start + index * PAGE_SIZE, 0),
                1 => second.insert_chunk(start + index * PAGE_SIZE, 0),
                _ => global.insert_chunk(start + index * PAGE_SIZE, 0),
            }
        }
        assert!(first.alloc_chunk(3).is_none());
        assert!(second.alloc_chunk(3).is_none());
        assert!(global.alloc_chunk(3).is_none());
        drain_to(&mut first, &mut global);
        drain_to(&mut second, &mut global);
        let first_free = first.total_size();
        let second_free = second.total_size();
        let allocated = global.alloc_chunk(4);
        let remaining = global.total_size();
        // Return all mock frames to unused state before any RED assertion.
        drain(&mut first, start);
        drain(&mut second, start);
        drain(&mut global, start);
        assert_eq!(
            allocated,
            Some(start),
            "CPU-local free pages must coalesce in the global pool"
        );
        assert_eq!(first_free, 0);
        assert_eq!(second_free, 0);
        assert_eq!(remaining, 0);
    }
}
