// SPDX-License-Identifier: MPL-2.0

//! Volatile generations for confirmed SSD baseline mappings.

use super::index::PageKey;
use crate::prelude::*;

const BASELINE_SHARDS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BaselineRangeToken {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) logical_start: u64,
    pub(crate) generations: Vec<u64>,
}

pub(crate) struct BaselineGenerationMap {
    shards: Box<[SpinLock<BTreeMap<PageKey, u64>>]>,
    owners: Box<[SpinLock<BTreeSet<PageKey>>]>,
}

impl BaselineGenerationMap {
    pub(crate) fn new() -> Result<Self> {
        let mut shards = Vec::new();
        shards
            .try_reserve_exact(BASELINE_SHARDS)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..BASELINE_SHARDS {
            shards.push(SpinLock::new(BTreeMap::new()));
        }
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(BASELINE_SHARDS)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..BASELINE_SHARDS {
            owners.push(SpinLock::new(BTreeSet::new()));
        }
        Ok(Self {
            shards: shards.into_boxed_slice(),
            owners: owners.into_boxed_slice(),
        })
    }

    pub(crate) fn snapshot(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Result<BaselineRangeToken> {
        let mut generations = Vec::new();
        generations
            .try_reserve_exact(pages)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for offset in 0..pages {
            let page_index = logical_start
                .checked_add(offset as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let key = PageKey::new(fs_id, inode, page_index);
            generations.push(
                self.shards[self.shard(key)]
                    .lock()
                    .get(&key)
                    .copied()
                    .unwrap_or(0),
            );
        }
        Ok(BaselineRangeToken {
            fs_id,
            inode,
            logical_start,
            generations,
        })
    }

    pub(crate) fn validate(&self, token: &BaselineRangeToken) -> bool {
        token
            .generations
            .iter()
            .enumerate()
            .all(|(offset, expected)| {
                let Some(page_index) = token.logical_start.checked_add(offset as u64) else {
                    return false;
                };
                let key = PageKey::new(token.fs_id, token.inode, page_index);
                self.shards[self.shard(key)]
                    .lock()
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    == *expected
            })
    }

    pub(crate) fn advance(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Result<()> {
        for offset in 0..pages {
            let page_index = logical_start
                .checked_add(offset as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let key = PageKey::new(fs_id, inode, page_index);
            let mut shard = self.shards[self.shard(key)].lock();
            let generation = shard.entry(key).or_insert(0);
            *generation = generation
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        }
        Ok(())
    }

    pub(crate) fn try_claim_migration(&self, key: PageKey) -> bool {
        self.owners[self.shard(key)].lock().insert(key)
    }

    pub(crate) fn release_migration(&self, key: PageKey) {
        self.owners[self.shard(key)].lock().remove(&key);
    }

    fn shard(&self, key: PageKey) -> usize {
        let owner = key.fs_id.rotate_left(13) ^ key.inode.rotate_left(31) ^ key.page_index;
        owner as usize % self.shards.len()
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
}
