// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::metrics;
use crate::prelude::*;

const NO_GROUP: usize = usize::MAX;

pub(in crate::fs::fs_impls::ext4) struct AllocationGroupSummary {
    packed: AtomicU64,
}

impl AllocationGroupSummary {
    pub(in crate::fs::fs_impls::ext4) const fn new(
        free_blocks: u32,
        longest_free_run: u32,
    ) -> Self {
        Self {
            packed: AtomicU64::new(Self::pack(free_blocks, longest_free_run)),
        }
    }

    const fn pack(free_blocks: u32, longest_free_run: u32) -> u64 {
        ((free_blocks as u64) << 32) | longest_free_run as u64
    }

    pub(in crate::fs::fs_impls::ext4) fn update(&self, free_blocks: u32, longest_free_run: u32) {
        self.packed
            .store(Self::pack(free_blocks, longest_free_run), Ordering::Release);
    }

    pub(in crate::fs::fs_impls::ext4) fn load(&self) -> (u32, u32) {
        let packed = self.packed.load(Ordering::Acquire);
        ((packed >> 32) as u32, packed as u32)
    }

    pub(in crate::fs::fs_impls::ext4) fn can_fit(&self, count: u32) -> bool {
        let (free_blocks, longest_free_run) = self.load();
        count != 0 && free_blocks >= count && longest_free_run >= count
    }
}

pub(in crate::fs::fs_impls::ext4) struct AllocationGroupSelector {
    summaries: Vec<Arc<AllocationGroupSummary>>,
    home_cursors: Vec<AtomicUsize>,
    last_success: Vec<AtomicUsize>,
    steal_cursor: AtomicUsize,
    lanes: usize,
}

impl AllocationGroupSelector {
    pub(in crate::fs::fs_impls::ext4) fn new(nr_groups: usize, nr_cpus: usize) -> Self {
        assert!(nr_groups != 0);
        Self::from_summaries(
            (0..nr_groups)
                .map(|_| Arc::new(AllocationGroupSummary::new(0, 0)))
                .collect(),
            nr_cpus,
        )
    }

    pub(in crate::fs::fs_impls::ext4) fn from_summaries(
        summaries: Vec<Arc<AllocationGroupSummary>>,
        nr_cpus: usize,
    ) -> Self {
        assert!(!summaries.is_empty());
        let nr_groups = summaries.len();
        let lanes = nr_cpus.max(1).min(nr_groups);
        Self {
            summaries,
            home_cursors: (0..lanes).map(|_| AtomicUsize::new(0)).collect(),
            last_success: (0..lanes).map(|_| AtomicUsize::new(NO_GROUP)).collect(),
            steal_cursor: AtomicUsize::new(0),
            lanes,
        }
    }

    pub(in crate::fs::fs_impls::ext4) fn update_summary(
        &self,
        group: usize,
        free_blocks: u32,
        longest_free_run: u32,
    ) {
        self.summaries[group].update(free_blocks, longest_free_run);
    }

    pub(in crate::fs::fs_impls::ext4) fn record_success(&self, cpu: usize, group: usize) {
        debug_assert!(group < self.summaries.len());
        self.last_success[cpu % self.lanes].store(group, Ordering::Release);
    }

    pub(in crate::fs::fs_impls::ext4) fn candidates(
        &self,
        cpu: usize,
        goal_group: Option<usize>,
        count: u32,
    ) -> AllocationGroupCandidates<'_> {
        let lane = cpu % self.lanes;
        let home_count = (self.summaries.len() - 1 - lane) / self.lanes + 1;
        AllocationGroupCandidates {
            selector: self,
            count,
            goal: goal_group.filter(|group| *group < self.summaries.len()),
            last: match self.last_success[lane].load(Ordering::Acquire) {
                NO_GROUP => None,
                group => Some(group),
            },
            lane,
            home_start: self.home_cursors[lane].fetch_add(1, Ordering::Relaxed) % home_count,
            home_offset: 0,
            home_count,
            steal_start: self.steal_cursor.fetch_add(1, Ordering::Relaxed) % self.summaries.len(),
            steal_offset: 0,
            stage: 0,
        }
    }
}

pub(in crate::fs::fs_impls::ext4) struct AllocationGroupCandidates<'a> {
    selector: &'a AllocationGroupSelector,
    count: u32,
    goal: Option<usize>,
    last: Option<usize>,
    lane: usize,
    home_start: usize,
    home_offset: usize,
    home_count: usize,
    steal_start: usize,
    steal_offset: usize,
    stage: u8,
}

impl AllocationGroupCandidates<'_> {
    fn eligible(&self, group: usize) -> bool {
        self.selector.summaries[group].can_fit(self.count)
    }

    fn is_home(&self, group: usize) -> bool {
        group % self.selector.lanes == self.lane
    }
}

impl Iterator for AllocationGroupCandidates<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stage {
                0 => {
                    self.stage = 1;
                    if let Some(group) = self.goal
                        && self.eligible(group)
                    {
                        metrics::SELECTOR_HOME_CANDIDATES.record(1);
                        return Some(group);
                    }
                }
                1 => {
                    self.stage = 2;
                    if let Some(group) = self.last
                        && Some(group) != self.goal
                        && self.eligible(group)
                    {
                        metrics::SELECTOR_HOME_CANDIDATES.record(1);
                        return Some(group);
                    }
                }
                2 => {
                    while self.home_offset < self.home_count {
                        let position = (self.home_start + self.home_offset) % self.home_count;
                        self.home_offset += 1;
                        let group = self.lane + position * self.selector.lanes;
                        if Some(group) != self.goal
                            && Some(group) != self.last
                            && self.eligible(group)
                        {
                            metrics::SELECTOR_HOME_CANDIDATES.record(1);
                            return Some(group);
                        }
                    }
                    self.stage = 3;
                }
                3 => {
                    while self.steal_offset < self.selector.summaries.len() {
                        let group =
                            (self.steal_start + self.steal_offset) % self.selector.summaries.len();
                        self.steal_offset += 1;
                        if Some(group) != self.goal
                            && Some(group) != self.last
                            && !self.is_home(group)
                            && self.eligible(group)
                        {
                            metrics::SELECTOR_STEAL_CANDIDATES.record(1);
                            return Some(group);
                        }
                    }
                    self.stage = 4;
                }
                _ => return None,
            }
        }
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::vec;

    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn rnvpce_selector_uses_disjoint_cpu_stripes() {
        let cpu0 = AllocationGroupSelector::new(8, 2);
        let cpu1 = AllocationGroupSelector::new(8, 2);
        for selector in [&cpu0, &cpu1] {
            for group in 0..8 {
                selector.update_summary(group, 32, 32);
            }
        }

        assert_eq!(
            cpu0.candidates(0, None, 8).collect::<Vec<_>>(),
            vec![0, 2, 4, 6, 1, 3, 5, 7]
        );
        assert_eq!(
            cpu1.candidates(1, None, 8).collect::<Vec<_>>(),
            vec![1, 3, 5, 7, 0, 2, 4, 6]
        );
    }

    #[ktest]
    fn rnvpce_selector_prefers_goal_then_last_success() {
        let selector = AllocationGroupSelector::new(8, 2);
        for group in 0..8 {
            selector.update_summary(group, 32, 32);
        }
        selector.record_success(0, 4);

        assert_eq!(
            selector.candidates(0, Some(6), 8).collect::<Vec<_>>(),
            vec![6, 4, 0, 2, 1, 3, 5, 7]
        );
    }

    #[ktest]
    fn rnvpce_selector_steals_after_home_exhaustion() {
        let selector = AllocationGroupSelector::new(8, 2);
        for group in [1, 3, 5, 7] {
            selector.update_summary(group, 32, 32);
        }

        assert_eq!(
            selector.candidates(0, None, 8).collect::<Vec<_>>(),
            vec![1, 3, 5, 7]
        );
    }

    #[ktest]
    fn rnvpce_selector_ignores_busy_group_without_enospc() {
        let selector = AllocationGroupSelector::new(4, 2);
        selector.update_summary(0, 32, 32);
        selector.update_summary(2, 32, 32);

        let candidates = selector.candidates(0, None, 8).collect::<Vec<_>>();
        assert_eq!(candidates, vec![0, 2]);
        assert_eq!(candidates.into_iter().find(|group| *group != 0), Some(2));
    }

    #[ktest]
    fn rnvpce_summary_tracks_allocate_free_and_recovery() {
        let summary = AllocationGroupSummary::new(64, 48);
        assert!(summary.can_fit(48));
        assert!(!summary.can_fit(49));
        summary.update(16, 8);
        assert_eq!(summary.load(), (16, 8));
        assert!(summary.can_fit(8));
        assert!(!summary.can_fit(9));
        summary.update(64, 64);
        assert_eq!(summary.load(), (64, 64));
    }
}
