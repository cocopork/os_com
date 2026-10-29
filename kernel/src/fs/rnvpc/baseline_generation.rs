// SPDX-License-Identifier: MPL-2.0

//! Volatile generations and ownership for confirmed SSD baseline mappings.

use super::index::PageKey;
use crate::prelude::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BaselineRangeToken {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) logical_start: u64,
    pub(crate) generations: Vec<u64>,
    pub(crate) targets: Vec<u64>,
}

impl BaselineRangeToken {
    pub(crate) fn target(&self, page_index: u64) -> Option<u64> {
        let offset = page_index.checked_sub(self.logical_start)?;
        self.targets.get(usize::try_from(offset).ok()?).copied()
    }
}

#[derive(Default)]
struct BaselineState {
    generations: BTreeMap<PageKey, u64>,
    migrations: BTreeSet<PageKey>,
    mutations: BTreeSet<PageKey>,
}

pub(crate) struct BaselineGenerationMap {
    state: SpinLock<BaselineState>,
}

pub(crate) struct BaselineMutationGuard<'a> {
    map: &'a BaselineGenerationMap,
    keys: Vec<PageKey>,
}

#[derive(Debug)]
pub(crate) struct BaselineRangeClaim {
    keys: Vec<PageKey>,
    generations: Vec<u64>,
}

impl BaselineRangeClaim {
    pub(crate) fn generations(&self) -> &[u64] {
        &self.generations
    }

    pub(crate) fn keys(&self) -> &[PageKey] {
        &self.keys
    }
}

impl Drop for BaselineMutationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.map.state.lock();
        for key in &self.keys {
            state.mutations.remove(key);
        }
    }
}

impl BaselineGenerationMap {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            state: SpinLock::new(BaselineState::default()),
        })
    }

    #[cfg(ktest)]
    pub(crate) fn snapshot(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Result<BaselineRangeToken> {
        let state = self.state.lock();
        let mut generations = Vec::new();
        generations
            .try_reserve_exact(pages)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for offset in 0..pages {
            let page_index = logical_start
                .checked_add(offset as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            generations.push(
                state
                    .generations
                    .get(&PageKey::new(fs_id, inode, page_index))
                    .copied()
                    .unwrap_or(0),
            );
        }
        Ok(BaselineRangeToken {
            fs_id,
            inode,
            logical_start,
            generations,
            targets: vec![0; pages],
        })
    }

    pub(crate) fn claimed_token(
        &self,
        pages: &[(PageKey, u64, u64)],
    ) -> Result<BaselineRangeToken> {
        let Some((first, _, _)) = pages.first() else {
            return_errno!(Errno::EINVAL);
        };
        if pages.iter().enumerate().any(|(offset, (key, _, _))| {
            (key.fs_id, key.inode) != (first.fs_id, first.inode)
                || key.page_index != first.page_index + offset as u64
        }) {
            return_errno_with_message!(Errno::EINVAL, "baseline token pages are not contiguous");
        }
        Ok(BaselineRangeToken {
            fs_id: first.fs_id,
            inode: first.inode,
            logical_start: first.page_index,
            generations: pages.iter().map(|(_, generation, _)| *generation).collect(),
            targets: pages.iter().map(|(_, _, target)| *target).collect(),
        })
    }

    pub(crate) fn validate(&self, token: &BaselineRangeToken) -> bool {
        if token.generations.len() != token.targets.len() {
            return false;
        }
        let state = self.state.lock();
        token
            .generations
            .iter()
            .enumerate()
            .all(|(offset, expected)| {
                let Some(page_index) = token.logical_start.checked_add(offset as u64) else {
                    return false;
                };
                state
                    .generations
                    .get(&PageKey::new(token.fs_id, token.inode, page_index))
                    .copied()
                    .unwrap_or(0)
                    == *expected
            })
    }

    pub(crate) fn try_claim_migration(&self, key: PageKey) -> Option<u64> {
        let mut state = self.state.lock();
        if state.mutations.contains(&key) || !state.migrations.insert(key) {
            return None;
        }
        Some(state.generations.get(&key).copied().unwrap_or(0))
    }

    pub(crate) fn try_claim_range(&self, keys: &[PageKey]) -> Result<BaselineRangeClaim> {
        let Some(first) = keys.first().copied() else {
            return_errno!(Errno::EINVAL);
        };
        if keys.iter().enumerate().any(|(offset, key)| {
            (key.fs_id, key.inode) != (first.fs_id, first.inode)
                || first.page_index.checked_add(offset as u64) != Some(key.page_index)
        }) {
            return_errno_with_message!(Errno::EINVAL, "baseline claim is not contiguous");
        }
        let mut state = self.state.lock();
        if keys
            .iter()
            .any(|key| state.mutations.contains(key) || state.migrations.contains(key))
        {
            return_errno_with_message!(Errno::EBUSY, "SSD baseline range is already owned");
        }
        let generations = keys
            .iter()
            .map(|key| state.generations.get(key).copied().unwrap_or(0))
            .collect();
        state.migrations.extend(keys.iter().copied());
        Ok(BaselineRangeClaim {
            keys: keys.to_vec(),
            generations,
        })
    }

    pub(crate) fn release_range(&self, claim: BaselineRangeClaim) {
        let mut state = self.state.lock();
        for key in claim.keys {
            state.migrations.remove(&key);
        }
    }

    pub(crate) fn begin_mutation(&self, keys: &[PageKey]) -> Result<BaselineMutationGuard<'_>> {
        let unique = keys.iter().copied().collect::<BTreeSet<_>>();
        let mut state = self.state.lock();
        if unique
            .iter()
            .any(|key| state.migrations.contains(key) || state.mutations.contains(key))
        {
            return_errno_with_message!(Errno::EBUSY, "SSD baseline range is owned by migration");
        }
        if unique
            .iter()
            .any(|key| state.generations.get(key).copied().unwrap_or(0) == u64::MAX)
        {
            return_errno!(Errno::EOVERFLOW);
        }
        for key in &unique {
            state.mutations.insert(*key);
            let generation = state.generations.entry(*key).or_insert(0);
            *generation += 1;
        }
        drop(state);
        Ok(BaselineMutationGuard {
            map: self,
            keys: unique.into_iter().collect(),
        })
    }

    pub(crate) fn advance_claimed(&self, keys: &[PageKey]) -> Result<()> {
        let mut state = self.state.lock();
        if keys.iter().any(|key| !state.migrations.contains(key)) {
            return_errno_with_message!(Errno::ESTALE, "SSD baseline migration owner was lost");
        }
        if keys
            .iter()
            .any(|key| state.generations.get(key).copied().unwrap_or(0) == u64::MAX)
        {
            return_errno!(Errno::EOVERFLOW);
        }
        for key in keys {
            let generation = state.generations.entry(*key).or_insert(0);
            *generation += 1;
        }
        Ok(())
    }

    #[cfg(ktest)]
    pub(crate) fn advance(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Result<()> {
        let keys = (0..pages)
            .map(|offset| PageKey::new(fs_id, inode, logical_start + offset as u64))
            .collect::<Vec<_>>();
        drop(self.begin_mutation(&keys)?);
        Ok(())
    }

    pub(crate) fn release_migration(&self, key: PageKey) {
        self.state.lock().migrations.remove(&key);
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn baseline_generation_invalidates_only_overlapping_pages() {
        let generations = BaselineGenerationMap::new().unwrap();
        let token = generations.snapshot(1, 2, 10, 4).unwrap();
        generations.advance(1, 2, 12, 1).unwrap();
        assert!(!generations.validate(&token));
        assert!(generations.validate(&generations.snapshot(1, 2, 10, 2).unwrap()));
        assert!(generations.validate(&generations.snapshot(1, 2, 13, 1).unwrap()));
    }

    #[ktest]
    fn baseline_mutation_and_migration_claim_exclude_each_other() {
        let generations = BaselineGenerationMap::new().unwrap();
        let key = PageKey::new(1, 2, 10);
        let generation = generations.try_claim_migration(key).unwrap();
        assert_eq!(generation, 0);
        assert!(generations.begin_mutation(&[key]).is_err());
        generations.release_migration(key);
        let guard = generations.begin_mutation(&[key]).unwrap();
        assert!(generations.try_claim_migration(key).is_none());
        drop(guard);
        assert_eq!(generations.try_claim_migration(key), Some(1));
    }

    #[ktest]
    fn claimed_generation_overflow_does_not_partially_advance_batch() {
        let generations = BaselineGenerationMap::new().unwrap();
        let first = PageKey::new(1, 2, 10);
        let overflow = PageKey::new(1, 2, 11);
        {
            let mut state = generations.state.lock();
            state.generations.insert(first, 7);
            state.generations.insert(overflow, u64::MAX);
            state.migrations.insert(first);
            state.migrations.insert(overflow);
        }
        assert_eq!(
            generations
                .advance_claimed(&[first, overflow])
                .unwrap_err()
                .error(),
            Errno::EOVERFLOW
        );
        let state = generations.state.lock();
        assert_eq!(state.generations.get(&first), Some(&7));
        assert_eq!(state.generations.get(&overflow), Some(&u64::MAX));
    }

    #[ktest]
    fn baseline_range_claim_is_atomic_and_releasable() {
        let generations = BaselineGenerationMap::new().unwrap();
        let keys = (10..14)
            .map(|page| PageKey::new(1, 2, page))
            .collect::<Vec<_>>();
        let claim = generations.try_claim_range(&keys).unwrap();
        assert_eq!(claim.generations(), &[0, 0, 0, 0]);
        assert_eq!(
            generations.try_claim_range(&keys).unwrap_err().error(),
            Errno::EBUSY
        );
        generations.release_range(claim);
        assert!(generations.try_claim_range(&keys).is_ok());
    }
}
