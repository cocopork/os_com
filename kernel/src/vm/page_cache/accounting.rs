// SPDX-License-Identifier: MPL-2.0

//! Optional filesystem-local accounting, including frames pinned outside the cache.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Counts resident, dirty and in-flight frames owned by a group of page caches.
///
/// Dirty and writeback may overlap when a writer modifies an in-flight page.
/// Their sum is a conservative pressure estimate. Counters are snapshots, not
/// synchronization primitives; admission must reserve its own headroom.
#[derive(Debug, Default)]
pub struct PageCacheAccounting {
    pub(super) resident: AtomicUsize,
    pub(super) dirty: AtomicUsize,
    pub(super) writeback: AtomicUsize,
}

impl PageCacheAccounting {
    /// Returns the number of live frames, including externally pinned frames.
    pub fn resident_pages(&self) -> usize {
        self.resident.load(Ordering::Acquire)
    }

    /// Returns the number of dirty frames.
    pub fn dirty_pages(&self) -> usize {
        self.dirty.load(Ordering::Acquire)
    }

    /// Returns the number of frames whose writeback has not completed.
    pub fn writeback_pages(&self) -> usize {
        self.writeback.load(Ordering::Acquire)
    }
}
