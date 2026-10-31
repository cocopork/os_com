// SPDX-License-Identifier: MPL-2.0

//! Atomic capacity accounting and pressure policy for RNVPC objects.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::config::RnvpcConfig;
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PoolClass {
    Metadata,
    Data,
    /// Per-inode WAL usage is capacity-only and owns no physical pool slot.
    Wal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RnvpcStats {
    pub(crate) total_pages: usize,
    pub(crate) free_pages: usize,
    pub(crate) metadata_pages: usize,
    pub(crate) data_pages: usize,
    pub(crate) wal_pages: usize,
    pub(crate) pinned_frames: usize,
    pub(crate) high_water_events: u64,
    pub(crate) backpressure_events: u64,
    pub(crate) high_water_pages: usize,
    pub(crate) low_water_pages: usize,
    pub(crate) flush_pressure: bool,
}

pub(super) struct CapacityState {
    total_pages: usize,
    data_limit_pages: usize,
    high_water_pages: usize,
    low_water_pages: usize,
    used_pages: AtomicUsize,
    metadata_pages: AtomicUsize,
    data_pages: AtomicUsize,
    wal_pages: AtomicUsize,
    above_high_water: AtomicBool,
    high_water_events: AtomicU64,
    backpressure_events: AtomicU64,
}

impl CapacityState {
    pub(super) fn new(config: RnvpcConfig) -> Self {
        let total_pages = config.total_pages();
        let data_limit_pages = config.data_limit_pages();
        let total_high_water_pages = (total_pages * 3).div_ceil(4);
        let high_water_pages = if data_limit_pages == 0 {
            total_high_water_pages
        } else {
            total_high_water_pages.min(data_limit_pages)
        };
        Self {
            total_pages,
            data_limit_pages,
            high_water_pages,
            low_water_pages: high_water_pages / 2,
            used_pages: AtomicUsize::new(0),
            metadata_pages: AtomicUsize::new(0),
            data_pages: AtomicUsize::new(0),
            wal_pages: AtomicUsize::new(0),
            above_high_water: AtomicBool::new(false),
            high_water_events: AtomicU64::new(0),
            backpressure_events: AtomicU64::new(0),
        }
    }

    /// Reserves configured PM capacity and reports a new data high-water event.
    pub(super) fn reserve(&self, class: PoolClass, pages: usize) -> Result<bool> {
        if pages == 0 {
            return Ok(false);
        }
        if class == PoolClass::Data
            && self
                .data_pages
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    (pages <= self.data_limit_pages.saturating_sub(used)).then_some(used + pages)
                })
                .is_err()
        {
            self.backpressure_events.fetch_add(1, Ordering::Relaxed);
            return_errno_with_message!(Errno::EAGAIN, "RNVPC data hard limit reached");
        }
        if self
            .used_pages
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (pages <= self.total_pages.saturating_sub(used)).then_some(used + pages)
            })
            .is_err()
        {
            if class == PoolClass::Data {
                self.data_pages.fetch_sub(pages, Ordering::AcqRel);
            }
            return_errno_with_message!(Errno::ENOSPC, "RNVPC capacity is exhausted");
        }
        match class {
            PoolClass::Metadata => {
                self.metadata_pages.fetch_add(pages, Ordering::AcqRel);
            }
            PoolClass::Data => {}
            PoolClass::Wal => {
                self.wal_pages.fetch_add(pages, Ordering::AcqRel);
            }
        }
        Ok(class == PoolClass::Data && self.note_high_water())
    }

    pub(super) fn release(&self, class: PoolClass, pages: usize) {
        if pages == 0 {
            return;
        }
        let previous_class = match class {
            PoolClass::Metadata => self.metadata_pages.fetch_sub(pages, Ordering::AcqRel),
            PoolClass::Data => self.data_pages.fetch_sub(pages, Ordering::AcqRel),
            PoolClass::Wal => self.wal_pages.fetch_sub(pages, Ordering::AcqRel),
        };
        debug_assert!(previous_class >= pages);
        let previous_used = self.used_pages.fetch_sub(pages, Ordering::AcqRel);
        debug_assert!(previous_used >= pages);
        if class == PoolClass::Data && previous_class - pages <= self.low_water_pages {
            self.above_high_water.store(false, Ordering::Release);
        }
    }

    pub(super) fn can_reserve_data(&self, pages: usize) -> bool {
        pages <= self.data_limit_pages.saturating_sub(self.data_pages())
            && pages <= self.total_pages.saturating_sub(self.used_pages())
    }

    pub(super) fn stats(&self) -> RnvpcStats {
        let used_pages = self.used_pages();
        RnvpcStats {
            total_pages: self.total_pages,
            free_pages: self.total_pages.saturating_sub(used_pages),
            metadata_pages: self.metadata_pages.load(Ordering::Acquire),
            data_pages: self.data_pages(),
            wal_pages: self.wal_pages.load(Ordering::Acquire),
            pinned_frames: 0,
            high_water_events: self.high_water_events.load(Ordering::Relaxed),
            backpressure_events: self.backpressure_events.load(Ordering::Relaxed),
            high_water_pages: self.high_water_pages,
            low_water_pages: self.low_water_pages,
            flush_pressure: self.above_high_water.load(Ordering::Acquire),
        }
    }

    fn note_high_water(&self) -> bool {
        if self.data_pages() < self.high_water_pages {
            return false;
        }
        if self
            .above_high_water
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.high_water_events.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn used_pages(&self) -> usize {
        self.used_pages.load(Ordering::Acquire)
    }

    fn data_pages(&self) -> usize {
        self.data_pages.load(Ordering::Acquire)
    }
}
