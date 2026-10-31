// SPDX-License-Identifier: MPL-2.0

//! Volatile physical-slot allocation for the RNVPC pool.

use core::{
    ops::Range,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use ostd::cpu::num_cpus;

use crate::prelude::*;

const MIN_DOMAIN_PAGES: usize = 1_024;
const AUTHORITATIVE_RETRIES: usize = 2;

pub(super) struct NvSlotAllocator {
    domains: Box<[NvAllocationDomain]>,
    total_pages: usize,
}

struct NvAllocationDomain {
    start: usize,
    end: usize,
    state: SpinLock<NvDomainState>,
    free_pages: AtomicUsize,
    longest_free_range: AtomicUsize,
    generation: AtomicU64,
}

struct NvDomainState {
    free_ranges: FreeRangeIndex,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NvSlotAllocatorStats {
    pub(super) total_pages: usize,
    pub(super) free_pages: usize,
    pub(super) domains: usize,
}

#[derive(Clone, Copy, Default)]
struct RangeSummary {
    prefix: usize,
    suffix: usize,
    longest: usize,
    len: usize,
}

impl RangeSummary {
    fn merge(left: Self, right: Self) -> Self {
        Self {
            prefix: if left.prefix == left.len {
                left.len + right.prefix
            } else {
                left.prefix
            },
            suffix: if right.suffix == right.len {
                right.len + left.suffix
            } else {
                right.suffix
            },
            longest: left
                .longest
                .max(right.longest)
                .max(left.suffix + right.prefix),
            len: left.len + right.len,
        }
    }

    fn word(bits: u64, len: usize) -> Self {
        let mask = if len == 64 {
            u64::MAX
        } else {
            (1u64 << len) - 1
        };
        let bits = bits & mask;
        let prefix = bits.trailing_ones().min(len as u32) as usize;
        let suffix = if len == 64 {
            bits.leading_ones() as usize
        } else {
            (bits << (64 - len)).leading_ones().min(len as u32) as usize
        };
        let mut remaining = bits;
        let mut longest = 0;
        while remaining != 0 {
            longest += 1;
            remaining &= remaining << 1;
        }
        Self {
            prefix,
            suffix,
            longest,
            len,
        }
    }
}

struct FreeRangeIndex {
    words: Vec<u64>,
    tree: Vec<RangeSummary>,
    leaves: usize,
    len: usize,
    free_pages: usize,
}

impl FreeRangeIndex {
    fn new(len: usize) -> Result<Self> {
        if len == 0 {
            return_errno!(Errno::EINVAL);
        }
        let word_count = len.div_ceil(64);
        let leaves = word_count.next_power_of_two();
        let mut index = Self {
            words: vec![u64::MAX; word_count],
            tree: vec![RangeSummary::default(); 2 * leaves],
            leaves,
            len,
            free_pages: len,
        };
        let tail = len % 64;
        if tail != 0 {
            index.words[word_count - 1] = (1u64 << tail) - 1;
        }
        for word in 0..word_count {
            index.update(word);
        }
        Ok(index)
    }

    fn longest(&self) -> usize {
        self.tree[1].longest
    }

    fn find(&self, count: usize, goal: usize) -> Option<Range<usize>> {
        if count == 0 || self.longest() < count {
            return None;
        }
        let mut carry = 0;
        let start = self
            .search(1, 0, goal.min(self.len), count, &mut carry)
            .or_else(|| {
                carry = 0;
                self.search(1, 0, 0, count, &mut carry)
            })?;
        Some(start..start + count)
    }

    fn allocate(&mut self, range: Range<usize>) -> Result<()> {
        self.set_range(range, false)
    }

    fn free(&mut self, range: Range<usize>) -> Result<()> {
        self.set_range(range, true)
    }

    fn set_range(&mut self, range: Range<usize>, free: bool) -> Result<()> {
        if range.is_empty() || range.end > self.len {
            return_errno!(Errno::EINVAL);
        }
        for slot in range.clone() {
            if self.is_free(slot) != !free {
                return_errno!(Errno::EINVAL);
            }
        }
        for slot in range.clone() {
            let word = slot / 64;
            let bit = slot % 64;
            if free {
                self.words[word] |= 1 << bit;
            } else {
                self.words[word] &= !(1 << bit);
            }
        }
        for word in range.start / 64..range.end.div_ceil(64) {
            self.update(word);
        }
        if free {
            self.free_pages += range.len();
        } else {
            self.free_pages -= range.len();
        }
        Ok(())
    }

    fn is_free(&self, slot: usize) -> bool {
        self.words[slot / 64] & (1 << (slot % 64)) != 0
    }

    fn update(&mut self, word: usize) {
        let len = (self.len - word * 64).min(64);
        let mut node = self.leaves + word;
        self.tree[node] = RangeSummary::word(self.words[word], len);
        while node > 1 {
            node /= 2;
            self.tree[node] = RangeSummary::merge(self.tree[2 * node], self.tree[2 * node + 1]);
        }
    }

    fn search(
        &self,
        node: usize,
        start: usize,
        goal: usize,
        count: usize,
        carry: &mut usize,
    ) -> Option<usize> {
        let summary = self.tree[node];
        if start + summary.len <= goal {
            return None;
        }
        if start >= goal {
            if *carry + summary.prefix >= count {
                return Some(start - *carry);
            }
            if summary.longest < count {
                *carry = if summary.suffix == summary.len {
                    *carry + summary.len
                } else {
                    summary.suffix
                };
                return None;
            }
        }
        if node >= self.leaves {
            let bits = self.words[node - self.leaves];
            for offset in goal.saturating_sub(start)..summary.len {
                if bits & (1 << offset) != 0 {
                    *carry += 1;
                } else {
                    *carry = 0;
                }
                if *carry >= count {
                    return Some(start + offset + 1 - count);
                }
            }
            return None;
        }
        self.search(2 * node, start, goal, count, carry)
            .or_else(|| {
                self.search(
                    2 * node + 1,
                    start + self.tree[2 * node].len,
                    goal,
                    count,
                    carry,
                )
            })
    }
}

impl NvSlotAllocator {
    pub(super) fn new(total_pages: usize) -> Result<Self> {
        if total_pages == 0 {
            return_errno!(Errno::EINVAL);
        }
        let candidate_domains = (total_pages / MIN_DOMAIN_PAGES).max(1);
        let domain_limit = num_cpus().min(candidate_domains);
        let domain_count = floor_power_of_two(domain_limit.max(1));
        let base_len = total_pages / domain_count;
        let remainder = total_pages % domain_count;
        let mut domains = Vec::new();
        domains
            .try_reserve_exact(domain_count)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        let mut start = 0;
        for index in 0..domain_count {
            let len = base_len + usize::from(index < remainder);
            let state = NvDomainState {
                free_ranges: FreeRangeIndex::new(len)?,
                cursor: 0,
            };
            domains.push(NvAllocationDomain {
                start,
                end: start + len,
                state: SpinLock::new(state),
                free_pages: AtomicUsize::new(len),
                longest_free_range: AtomicUsize::new(len),
                generation: AtomicU64::new(0),
            });
            start += len;
        }
        Ok(Self {
            domains: domains.into_boxed_slice(),
            total_pages,
        })
    }

    pub(super) fn allocate_contiguous(
        &self,
        pages: usize,
        hint: usize,
    ) -> Result<Option<Range<usize>>> {
        if pages == 0 {
            return Ok(Some(0..0));
        }
        if pages > self.total_pages {
            return Ok(None);
        }
        if let Some(range) = self.try_allocate_exact(pages, hint, true)? {
            return Ok(Some(range));
        }
        for _ in 0..AUTHORITATIVE_RETRIES {
            let before = self.generations();
            let allocation = self.try_allocate_exact(pages, hint, false)?;
            let stable = before == self.generations();
            if allocation.is_some() || stable {
                return Ok(allocation);
            }
        }
        return_errno!(Errno::EAGAIN)
    }

    pub(super) fn allocate_ranges(&self, pages: usize, hint: usize) -> Result<Vec<Range<usize>>> {
        if let Some(range) = self.allocate_contiguous(pages, hint)? {
            return Ok(if range.is_empty() {
                Vec::new()
            } else {
                vec![range]
            });
        }
        let mut remaining = pages;
        let mut ranges = Vec::new();
        for offset in 0..self.domains.len() {
            let index = (hint + offset) % self.domains.len();
            let domain = &self.domains[index];
            let mut state = domain.state.lock();
            let count = remaining.min(state.free_ranges.longest());
            if count == 0 {
                continue;
            }
            let Some(local) = state.free_ranges.find(count, state.cursor) else {
                continue;
            };
            state.free_ranges.allocate(local.clone())?;
            state.cursor = local.end % (domain.end - domain.start);
            Self::publish_domain_state(domain, &state);
            ranges.push(domain.start + local.start..domain.start + local.end);
            remaining -= count;
            if remaining == 0 {
                return Ok(ranges);
            }
        }
        for range in ranges {
            self.free_range(range)?;
        }
        return_errno!(Errno::ENOSPC)
    }

    pub(super) fn free_range(&self, range: Range<usize>) -> Result<()> {
        let (domain, local) = self.resolve_range(range)?;
        let mut state = domain.state.lock();
        state.free_ranges.free(local)?;
        Self::publish_domain_state(domain, &state);
        Ok(())
    }

    pub(super) fn free_slots(&self, slots: &[usize]) -> Result<()> {
        if slots.is_empty() {
            return Ok(());
        }
        let mut sorted = slots.to_vec();
        sorted.sort_unstable();
        if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
            return_errno!(Errno::EINVAL);
        }
        let mut start = sorted[0];
        let mut previous = start;
        for slot in sorted.into_iter().skip(1) {
            let domain_end = self.domain_for_slot(previous)?.end;
            if slot != previous + 1 || slot >= domain_end {
                self.free_range(start..previous + 1)?;
                start = slot;
            }
            previous = slot;
        }
        self.free_range(start..previous + 1)
    }

    pub(super) fn mark_allocated(&self, range: Range<usize>) -> Result<()> {
        let (domain, local) = self.resolve_range(range)?;
        let mut state = domain.state.lock();
        state.free_ranges.allocate(local)?;
        Self::publish_domain_state(domain, &state);
        Ok(())
    }

    pub(super) fn stats(&self) -> NvSlotAllocatorStats {
        NvSlotAllocatorStats {
            total_pages: self.total_pages,
            free_pages: self
                .domains
                .iter()
                .map(|domain| domain.free_pages.load(Ordering::Acquire))
                .sum(),
            domains: self.domains.len(),
        }
    }

    fn try_allocate_exact(
        &self,
        pages: usize,
        hint: usize,
        use_summary: bool,
    ) -> Result<Option<Range<usize>>> {
        for offset in 0..self.domains.len() {
            let index = (hint + offset) % self.domains.len();
            let domain = &self.domains[index];
            if use_summary
                && (domain.free_pages.load(Ordering::Acquire) < pages
                    || domain.longest_free_range.load(Ordering::Acquire) < pages)
            {
                continue;
            }
            let mut state = domain.state.lock();
            let Some(local) = state.free_ranges.find(pages, state.cursor) else {
                continue;
            };
            state.free_ranges.allocate(local.clone())?;
            state.cursor = local.end % (domain.end - domain.start);
            Self::publish_domain_state(domain, &state);
            return Ok(Some(domain.start + local.start..domain.start + local.end));
        }
        Ok(None)
    }

    fn resolve_range(&self, range: Range<usize>) -> Result<(&NvAllocationDomain, Range<usize>)> {
        if range.is_empty() || range.end > self.total_pages {
            return_errno!(Errno::EINVAL);
        }
        let domain = self.domain_for_slot(range.start)?;
        if range.end > domain.end {
            return_errno_with_message!(Errno::EINVAL, "NV slot range crosses allocation domains");
        }
        Ok((domain, range.start - domain.start..range.end - domain.start))
    }

    fn domain_for_slot(&self, slot: usize) -> Result<&NvAllocationDomain> {
        self.domains
            .iter()
            .find(|domain| slot >= domain.start && slot < domain.end)
            .ok_or_else(|| Error::new(Errno::EINVAL))
    }

    fn publish_domain_state(domain: &NvAllocationDomain, state: &NvDomainState) {
        domain
            .free_pages
            .store(state.free_ranges.free_pages, Ordering::Release);
        domain
            .longest_free_range
            .store(state.free_ranges.longest(), Ordering::Release);
        domain.generation.fetch_add(1, Ordering::Release);
    }

    fn generations(&self) -> Vec<u64> {
        self.domains
            .iter()
            .map(|domain| domain.generation.load(Ordering::Acquire))
            .collect()
    }
}

fn floor_power_of_two(value: usize) -> usize {
    1usize << (usize::BITS - 1 - value.leading_zeros())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn rnvpce_slot_allocator() {
        contiguous_and_reuse();
        cross_domain_fallback();
        fragmented_split();
        model_stress();
    }

    fn contiguous_and_reuse() {
        let allocator = NvSlotAllocator::new(2_048).unwrap();
        let first = allocator.allocate_contiguous(32, 0).unwrap().unwrap();
        assert_eq!(first, 0..32);
        assert_eq!(allocator.stats().free_pages, 2_016);
        allocator.free_range(first.clone()).unwrap();
        assert_eq!(allocator.stats().free_pages, 2_048);
        assert_eq!(
            allocator.free_range(first).unwrap_err().error(),
            Errno::EINVAL
        );
    }

    fn cross_domain_fallback() {
        let allocator = NvSlotAllocator::new(2_048).unwrap();
        assert_eq!(allocator.stats().domains, 2);
        assert_eq!(
            allocator.allocate_contiguous(1_024, 0).unwrap(),
            Some(0..1_024)
        );
        assert_eq!(
            allocator.allocate_contiguous(8, 0).unwrap(),
            Some(1_024..1_032)
        );
    }

    fn fragmented_split() {
        let allocator = NvSlotAllocator::new(2_048).unwrap();
        allocator.mark_allocated(4..1_024).unwrap();
        allocator.mark_allocated(1_028..2_048).unwrap();
        assert_eq!(allocator.stats().free_pages, 8);
        assert_eq!(allocator.allocate_contiguous(8, 0).unwrap(), None);
        assert_eq!(
            allocator.allocate_ranges(8, 0).unwrap(),
            vec![0..4, 1_024..1_028]
        );
        assert_eq!(allocator.stats().free_pages, 0);
        assert_eq!(
            allocator.mark_allocated(0..1).unwrap_err().error(),
            Errno::EINVAL
        );
    }

    fn model_stress() {
        let allocator = NvSlotAllocator::new(2_048).unwrap();
        let mut allocated = BTreeSet::new();
        let mut seed = 7u32;
        for step in 0..4_096 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if step % 3 != 0 && allocated.len() < 2_048 {
                let range = allocator
                    .allocate_contiguous(1, seed as usize)
                    .unwrap()
                    .unwrap();
                assert_eq!(range.len(), 1);
                assert!(allocated.insert(range.start));
            } else if let Some(slot) = allocated.pop_first() {
                allocator.free_range(slot..slot + 1).unwrap();
            }
            assert_eq!(allocator.stats().free_pages, 2_048 - allocated.len());
        }
    }
}
