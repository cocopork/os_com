// SPDX-License-Identifier: MPL-2.0

use super::super::prelude::*;

/// A bounded segment tree over 64-block words. Bitmap remains authoritative.
pub(in crate::fs::fs_impls::ext4) struct FreeSpaceIndex {
    words: Vec<u64>,
    tree: Vec<Summary>,
    leaves: usize,
    len: u16,
}

#[derive(Clone, Copy, Default)]
struct Summary {
    prefix: u16,
    suffix: u16,
    longest: u16,
    len: u16,
}

impl Summary {
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

    fn word(bits: u64, len: u16) -> Self {
        let mask = u64::MAX >> (64 - len);
        let bits = bits & mask;
        let prefix = bits.trailing_ones().min(len as u32) as u16;
        let suffix = (bits << (64 - len)).leading_ones().min(len as u32) as u16;
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

impl FreeSpaceIndex {
    pub(in crate::fs::fs_impls::ext4) fn new(bitmap: &IdBitmap) -> Self {
        let len = bitmap.len();
        assert!(len > 0);
        let nwords = (len as usize).div_ceil(64);
        let leaves = nwords.next_power_of_two();
        let mut index = Self {
            words: vec![0; nwords],
            tree: vec![Summary::default(); 2 * leaves],
            leaves,
            len,
        };
        index.refresh(bitmap, 0..len);
        index
    }

    pub(in crate::fs::fs_impls::ext4) fn refresh(&mut self, bitmap: &IdBitmap, range: Range<u16>) {
        if range.is_empty() {
            return;
        }
        for word in range.start as usize / 64..(range.end as usize).div_ceil(64) {
            let start = word * 8;
            self.words[word] =
                !u64::from_le_bytes(bitmap.as_bytes()[start..start + 8].try_into().unwrap());
            self.update(word);
        }
    }

    pub(in crate::fs::fs_impls::ext4) fn longest_free_run(&self) -> u32 {
        u32::from(self.tree[1].longest)
    }

    pub(in crate::fs::fs_impls::ext4) fn exclude(&mut self, range: Range<u16>) {
        if range.is_empty() {
            return;
        }
        for word in range.start as usize / 64..(range.end as usize).div_ceil(64) {
            let lo = (range.start as usize).saturating_sub(word * 64);
            let hi = (range.end as usize - word * 64).min(64);
            self.words[word] &= !(u64::MAX << lo & u64::MAX >> (64 - hi));
            self.update(word);
        }
    }

    fn update(&mut self, word: usize) {
        let len = (self.len as usize - word * 64).min(64) as u16;
        let mut node = self.leaves + word;
        self.tree[node] = Summary::word(self.words[word], len);
        while node > 1 {
            node /= 2;
            self.tree[node] = Summary::merge(self.tree[2 * node], self.tree[2 * node + 1]);
        }
    }

    /// Finds the first run after goal, falling back to the group's head.
    pub(in crate::fs::fs_impls::ext4) fn find(
        &self,
        count: u16,
        goal: u16,
        partial: bool,
    ) -> Option<Range<u16>> {
        if count == 0 {
            return None;
        }
        let count = if partial {
            count.min(self.tree[1].longest)
        } else {
            count
        };
        if count == 0 || self.tree[1].longest < count {
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

    fn search(
        &self,
        node: usize,
        start: u16,
        goal: u16,
        count: u16,
        carry: &mut u16,
    ) -> Option<u16> {
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

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    fn bitmap(len: u16) -> IdBitmap {
        IdBitmap::from_buf(vec![0; BLOCK_SIZE].into_boxed_slice(), len)
    }

    #[ktest]
    fn ext4_free_space_index() {
        crate::fs::fs_impls::ext4::test_utils::scenarios!(
            group_index_goal_fragment_merge_and_tail,
            group_index_matches_bitmap_model,
        );
    }

    fn group_index_goal_fragment_merge_and_tail() {
        let mut bitmap = bitmap(131);
        let mut index = FreeSpaceIndex::new(&bitmap);
        assert_eq!(index.longest_free_run(), 131);
        assert_eq!(index.find(70, 60, false), Some(60..130));
        bitmap.alloc_exact_at(60, 70).unwrap();
        index.refresh(&bitmap, 60..130);
        assert_eq!(index.longest_free_run(), 60);
        assert_eq!(index.find(61, 0, false), None);
        assert_eq!(index.find(90, 0, true), Some(0..60));
        bitmap.free_consecutive(60..130);
        index.refresh(&bitmap, 60..130);
        assert_eq!(index.find(131, 0, false), Some(0..131));
        index.exclude(0..130);
        assert_eq!(index.find(2, 0, true), Some(130..131));
        index.refresh(&bitmap, 0..131);
        assert_eq!(index.longest_free_run(), 131);
        assert_eq!(index.find(131, 1, false), Some(0..131));
    }

    fn group_index_matches_bitmap_model() {
        let mut bitmap = bitmap(257);
        let mut index = FreeSpaceIndex::new(&bitmap);
        let mut seed = 7u32;
        for step in 0..2000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let start = (seed % 257) as u16;
            let end = (start + ((seed >> 16) % 23) as u16 + 1).min(257);
            if step % 3 == 0 {
                for bit in start..end {
                    if bitmap.is_allocated(bit) {
                        bitmap.free(bit);
                    }
                }
            } else {
                for bit in start..end {
                    if !bitmap.is_allocated(bit) {
                        bitmap.alloc_exact_at(bit, 1).unwrap();
                    }
                }
            }
            index.refresh(&bitmap, start..end);
            for count in [1, 2, 7, 63, 64, 65] {
                let expected = (0..=257 - count)
                    .find(|&s| (s..s + count).all(|b| !bitmap.is_allocated(b)))
                    .map(|s| s..s + count);
                assert_eq!(index.find(count, 0, false), expected);
            }
        }
    }
}
