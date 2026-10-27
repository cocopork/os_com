// SPDX-License-Identifier: MPL-2.0

use super::{
    descriptor::{NvDescriptor, NvObjectKind},
    id::NvPageId,
    metadata::PersistentObjectRef,
};
use crate::prelude::*;

/// 页的逻辑身份：`(filesystem_id, inode_id, page_index)`。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PageKey {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) page_index: u64,
}

impl PageKey {
    pub(crate) const fn new(fs_id: u64, inode: u64, page_index: u64) -> Self {
        Self {
            fs_id,
            inode,
            page_index,
        }
    }
}

/// Compact DRAM-only locator for an NVPage's owning leaf extent slot.
///
/// Bits 0..48 store `leaf_slot + 1`; bits 48..64 store the leaf entry index.
/// The leaf generation is deliberately not packed: migration validates that
/// the current slot still contains the exact NVPage reference before update,
/// so slot reuse produces a stale candidate instead of an unsafe mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReverseExtentRef(core::num::NonZeroU64);

impl ReverseExtentRef {
    const SLOT_BITS: u32 = 48;
    const SLOT_MASK: u64 = (1u64 << Self::SLOT_BITS) - 1;

    pub(crate) fn new(leaf_slot: u64, entry_index: usize) -> Result<Self> {
        let slot_plus_one = leaf_slot
            .checked_add(1)
            .filter(|value| *value <= Self::SLOT_MASK)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let entry_index = u16::try_from(entry_index).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        Ok(Self(
            core::num::NonZeroU64::new(slot_plus_one | ((entry_index as u64) << Self::SLOT_BITS))
                .ok_or_else(|| Error::new(Errno::EINVAL))?,
        ))
    }

    pub(crate) const fn leaf_slot(self) -> u64 {
        (self.0.get() & Self::SLOT_MASK) - 1
    }

    pub(crate) const fn entry_index(self) -> usize {
        (self.0.get() >> Self::SLOT_BITS) as usize
    }
}

const _: () = assert!(size_of::<ReverseExtentRef>() == 8);
const _: () = assert!(size_of::<Option<ReverseExtentRef>>() == 8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DirtyNvPage {
    pub(crate) page: NvPageId,
    pub(crate) allocation_seq: u64,
}

/// 页当前驻留在哪一侧。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Residency {
    /// 只在 DRAM 控制面（掉电后不恢复）。
    Dram,
    /// 持久数据面（NV/PM）。
    Nv,
}

/// 运行时句柄：DRAM 页 vs NV 对象。持久化时**不写**（`T3.2`：DRAM 控制面不入盘）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeHandle {
    /// DRAM 页的运行时标识（由 DRAM 分配器给出）。
    Dram(u64),
    /// NV 对象在池中的槽位。
    Nv(NvPageId),
}

/// B-tree 叶子。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageEntry {
    pub(crate) residency: Residency,
    pub(crate) handle: Option<RuntimeHandle>,
    /// Exact persistent generation selected for this logical page.
    pub(crate) reference: PersistentObjectRef,
    /// Monotonic version within one [`PageKey`].
    pub(crate) logical_version: u64,
    /// 该页在 SSD 上的物理位置（NV 侧自带；Dram-only 页为 0）。
    pub(crate) ssd_backing_block: u64,
    /// 生命周期状态（`descriptor::NvObjectState`，与 `design.md §4.4` 同一套）。
    pub(crate) state: super::descriptor::NvObjectState,
}

/// 重建计数（T3.3 验收：恢复后能看到"重建了多少 Nv entry、丢了多少 Dram entry"）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexCounters {
    pub(crate) nv_entries: u64,
    pub(crate) dram_entries: u64,
    pub(crate) rebuilt_nv_entries: u64,
    pub(crate) dropped_dram_entries: u64,
}

/// DRAM 控制面的页索引。
#[derive(Default)]
pub(crate) struct PageIndex {
    entries: BTreeMap<PageKey, PageEntry>,
    counters: IndexCounters,
}

impl PageIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 插入/覆盖一个 entry（`Dram` 与 `Nv` 共用同一棵树，见 T3.1）。
    pub(crate) fn insert(&mut self, key: PageKey, entry: PageEntry) {
        match entry.residency {
            Residency::Nv => self.counters.nv_entries += 1,
            Residency::Dram => self.counters.dram_entries += 1,
        }
        if let Some(previous) = self.entries.insert(key, entry) {
            match previous.residency {
                Residency::Nv => self.counters.nv_entries -= 1,
                Residency::Dram => self.counters.dram_entries -= 1,
            }
        }
    }

    pub(crate) fn lookup(&self, key: PageKey) -> Option<&PageEntry> {
        self.entries.get(&key)
    }

    pub(crate) fn lookup_mut(&mut self, key: PageKey) -> Option<&mut PageEntry> {
        self.entries.get_mut(&key)
    }

    pub(crate) fn remove(&mut self, key: PageKey) -> Option<PageEntry> {
        let removed = self.entries.remove(&key)?;
        match removed.residency {
            Residency::Nv => self.counters.nv_entries -= 1,
            Residency::Dram => self.counters.dram_entries -= 1,
        }
        Some(removed)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn counters(&self) -> IndexCounters {
        self.counters
    }

    /// 逐 key 迭代（顺序即 `(fs_id, inode, page_index)` 字典序）——
    /// T5.4 的候选队列、T3.3 的计数核对都用它。
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&PageKey, &PageEntry)> {
        self.entries.iter()
    }

    /// T3.3：从持久数据面（自描述描述符）**顺序扫描重建**。
    ///
    /// - 只产出 `Nv` entries（`NvPage` 描述符）；
    /// - `Dram` entries 一个都不建——它们不是持久数据，掉电即失效
    ///   （`design-evolution §3`：恢复输入只含持久数据面）；
    /// - 计数记录进 `IndexCounters`，供恢复后的自检与实验统计。
    pub(crate) fn rebuild_from_descriptors(&mut self, descriptors: &[NvDescriptor]) -> Result<()> {
        for descriptor in descriptors {
            if descriptor.kind()? != NvObjectKind::NvPage {
                continue;
            }
            let handle = NvPageId::try_from(descriptor.ssd_backing_block as usize)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            let key = PageKey::new(descriptor.fs_id, descriptor.inode, descriptor.logical_index);
            self.insert(
                key,
                PageEntry {
                    residency: Residency::Nv,
                    handle: Some(RuntimeHandle::Nv(handle)),
                    reference: PersistentObjectRef::NONE,
                    logical_version: descriptor.recovery_id,
                    ssd_backing_block: descriptor.ssd_backing_block,
                    state: descriptor.state()?,
                },
            );
            self.counters.rebuilt_nv_entries += 1;
        }
        Ok(())
    }

    /// T3.3：丢弃所有 `Dram` entries（恢复语义：DRAM 侧没有任何东西可以复用）。
    pub(crate) fn drop_dram_entries(&mut self) -> usize {
        let keys: Vec<PageKey> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| (entry.residency == Residency::Dram).then_some(*key))
            .collect();
        for key in &keys {
            let _ = self.remove(*key);
        }
        self.counters.dropped_dram_entries += keys.len() as u64;
        keys.len()
    }

    /// 待写回的候选（T5.4 的输入）：`NvDirty` 且在 NV 侧。
    pub(crate) fn dirty_candidates(&self) -> Vec<PageKey> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| {
                (entry.residency == Residency::Nv
                    && entry.state == super::descriptor::NvObjectState::NvDirty)
                    .then_some(*key)
            })
            .collect()
    }
}

#[cfg(any(test, ktest))]
mod recovery_tests {
    use super::*;

    #[cfg_attr(ktest, ostd::prelude::ktest)]
    #[cfg_attr(not(ktest), test)]
    fn recovered_reverse_reference_packing_checks_boundaries() {
        assert_eq!(size_of::<Option<ReverseExtentRef>>(), 8);
        let reference = ReverseExtentRef::new((1 << 48) - 2, u16::MAX as usize).unwrap();
        assert_eq!(reference.leaf_slot(), (1 << 48) - 2);
        assert_eq!(reference.entry_index(), u16::MAX as usize);
        assert!(ReverseExtentRef::new((1 << 48) - 1, 0).is_err());
        assert!(ReverseExtentRef::new(u64::MAX, 0).is_err());
        assert!(ReverseExtentRef::new(0, u16::MAX as usize + 1).is_err());
    }
}
