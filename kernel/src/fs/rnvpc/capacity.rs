// SPDX-License-Identifier: MPL-2.0

//! Capacity accounting and pressure policy for RNVPC objects.

use super::config::RnvpcConfig;
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PoolClass {
    Metadata,
    Data,
    /// per-inode WAL 占用的 PM 页（`porting.md §3.1`：WAL 计入 PM 容量）。
    Wal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RnvpcStats {
    pub(crate) total_pages: usize,
    pub(crate) free_pages: usize,
    pub(crate) metadata_pages: usize,
    pub(crate) data_pages: usize,
    /// WAL 折算出来的 PM 页数（T1.2）。
    pub(crate) wal_pages: usize,
    pub(crate) pinned_frames: usize,
    pub(crate) high_water_events: u64,
    pub(crate) backpressure_events: u64,
    /// rTIO v0：回刷水线（`min(3/4·total, data_limit)`），已用 ≥ 它就该开始回刷。
    pub(crate) high_water_pages: usize,
}

pub(super) struct CapacityState {
    total_pages: usize,
    data_limit_pages: usize,
    high_water_pages: usize,
    metadata_pages: usize,
    data_pages: usize,
    wal_pages: usize,
    above_high_water: bool,
    high_water_events: u64,
    backpressure_events: u64,
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
            metadata_pages: 0,
            data_pages: 0,
            wal_pages: 0,
            above_high_water: false,
            high_water_events: 0,
            backpressure_events: 0,
        }
    }

    pub(super) fn before_allocate(&mut self, class: PoolClass) -> Result<()> {
        if self.used_pages() == self.total_pages {
            return_errno_with_message!(Errno::ENOSPC, "RNVPC capacity is exhausted");
        }
        if class == PoolClass::Data && self.data_pages == self.data_limit_pages {
            self.backpressure_events += 1;
            return_errno_with_message!(Errno::EAGAIN, "RNVPC data hard limit reached");
        }
        Ok(())
    }

    pub(super) fn before_allocate_many(&mut self, class: PoolClass, pages: usize) -> Result<()> {
        if pages > self.total_pages.saturating_sub(self.used_pages()) {
            return_errno_with_message!(Errno::ENOSPC, "RNVPC capacity is exhausted");
        }
        if class == PoolClass::Data && pages > self.data_limit_pages.saturating_sub(self.data_pages)
        {
            self.backpressure_events += 1;
            return_errno_with_message!(Errno::EAGAIN, "RNVPC data hard limit reached");
        }
        Ok(())
    }

    pub(super) fn account_allocate(&mut self, class: PoolClass) -> bool {
        match class {
            PoolClass::Metadata => self.metadata_pages += 1,
            PoolClass::Data => self.data_pages += 1,
            PoolClass::Wal => self.wal_pages += 1,
        }

        if !self.above_high_water && self.used_pages() >= self.high_water_pages {
            self.above_high_water = true;
            self.high_water_events += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn account_recovered(&mut self, class: PoolClass) -> Result<()> {
        self.before_allocate(class)?;
        let _ = self.account_allocate(class);
        Ok(())
    }

    pub(super) fn account_free(&mut self, class: PoolClass) {
        match class {
            PoolClass::Metadata => self.metadata_pages -= 1,
            PoolClass::Data => self.data_pages -= 1,
            PoolClass::Wal => self.wal_pages -= 1,
        }
        if self.used_pages() < self.high_water_pages {
            self.above_high_water = false;
        }
    }

    pub(super) fn stats(&self) -> RnvpcStats {
        RnvpcStats {
            total_pages: self.total_pages,
            free_pages: self.total_pages - self.used_pages(),
            metadata_pages: self.metadata_pages,
            data_pages: self.data_pages,
            wal_pages: self.wal_pages,
            pinned_frames: 0, // Filled from the pool's independent pin counter.
            high_water_events: self.high_water_events,
            backpressure_events: self.backpressure_events,
            high_water_pages: self.high_water_pages,
        }
    }

    fn used_pages(&self) -> usize {
        self.metadata_pages + self.data_pages + self.wal_pages
    }
}
