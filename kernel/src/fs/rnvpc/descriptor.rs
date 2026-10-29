// SPDX-License-Identifier: MPL-2.0

use aster_block::BLOCK_SIZE;

use crate::prelude::*;

pub(crate) const NV_DESCRIPTOR_MAGIC: u64 = 0x4e56_4445_5343_3032;
/// 描述符版本。
pub(crate) const NV_DESCRIPTOR_VERSION: u32 = 1;
const NV_DESCRIPTOR_MIGRATION_TARGET: u16 = 1 << 0;
const NV_DESCRIPTOR_MIGRATION_CREDIT_RELEASED: u16 = 1 << 1;

/// 对象种类（`design.md §2.1/§2.2`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum NvObjectKind {
    /// 4 KiB 文件数据对象。
    NvPage = 1,
    /// EXT4 metadata buffer 对象（≤ 文件系统块）。
    Nvbh = 2,
}

impl NvObjectKind {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::NvPage),
            2 => Ok(Self::Nvbh),
            _ => return_errno_with_message!(Errno::EINVAL, "unknown NvObjectKind"),
        }
    }
}

/// 生命周期状态（`design.md §4.4`、`design-evolution §3`）。
///
/// 注意这里**没有** `NvReclaimable → DramClean` 的边：已写回的 clean NVPage 直接
/// 回收，不转成 DRAM page（`§6` 明确废弃）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum NvObjectState {
    /// 已落 SSD 且仍驻留 NV，可回收。
    NvReclaimable = 1,
    /// 已被修改，尚未提交 writeback。
    NvDirty = 2,
    /// 已提交 writeback，等在飞 BIO 完成。
    NvWriteback = 3,
    /// 只存在于 DRAM 控制面（掉电后不恢复）。
    DramClean = 4,
}

impl NvObjectState {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::NvReclaimable),
            2 => Ok(Self::NvDirty),
            3 => Ok(Self::NvWriteback),
            4 => Ok(Self::DramClean),
            _ => return_errno_with_message!(Errno::EINVAL, "unknown NvObjectState"),
        }
    }

    /// 允许的状态迁移（`design.md §4.4`）。
    ///
    /// - `DramClean` 被写 → CoW 出 `NvDirty`；
    /// - `NvDirty` 提交 → `NvWriteback`；
    /// - completion 成功 → `NvReclaimable`；失败 → 保留并重新排队（回 `NvDirty`）；
    /// - `NvReclaimable` 是终态（回收），**不**回落到 `DramClean`。
    pub(crate) const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::DramClean, Self::NvDirty)
                | (Self::NvDirty, Self::NvWriteback)
                | (Self::NvWriteback, Self::NvReclaimable)
                | (Self::NvWriteback, Self::NvDirty)
        )
    }

    /// 该状态是否允许出现在**持久数据面**上（供恢复扫描判断）。
    pub(crate) const fn is_persistable(self) -> bool {
        matches!(
            self,
            Self::NvDirty | Self::NvWriteback | Self::NvReclaimable
        )
    }
}

/// 持久对象描述符：自描述、可扫描恢复、无全局索引。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct NvDescriptor {
    pub(super) magic: u64,
    pub(super) version: u32,
    pub(super) kind: u8,
    pub(super) state: u8,
    pub(super) flags: u16,
    /// 所属文件系统。
    pub(super) fs_id: u64,
    /// NVPage：归属 inode；NVBH：所属 inode（0 = 不属于单个 inode 的全局 metadata）。
    pub(super) inode: u64,
    /// NVPage：page index；NVBH：metadata block 起始号。
    pub(super) logical_index: u64,
    /// NVPage：`PAGE_SIZE`；NVBH：实际字节数（`1..=BLOCK_SIZE`）。
    pub(super) length: u32,
    /// 引用计数（NVBH → NVPage 的引用，以及上层对 NVBH 的引用）。
    pub(super) refs: u32,
    /// 掉电恢复标识。
    pub(super) recovery_id: u64,
    /// 本对象在 SSD 上的物理位置（per-object，替代旧的统一编址）。
    pub(super) ssd_backing_block: u64,
    pub(super) reserved: u64,
}

const _: () = {
    assert!(size_of::<NvDescriptor>() == 72);
    assert!(align_of::<NvDescriptor>() == 8);
};

impl NvDescriptor {
    /// 构造一个 NVPage 描述符。
    pub(crate) fn for_nv_page(
        fs_id: u64,
        inode: u64,
        page_index: u64,
        ssd_backing_block: u64,
        recovery_id: u64,
    ) -> Self {
        Self {
            magic: NV_DESCRIPTOR_MAGIC,
            version: NV_DESCRIPTOR_VERSION,
            kind: NvObjectKind::NvPage as u8,
            state: NvObjectState::NvDirty as u8,
            flags: 0,
            fs_id,
            inode,
            logical_index: page_index,
            length: PAGE_SIZE as u32,
            refs: 0,
            recovery_id,
            ssd_backing_block,
            reserved: 0,
        }
    }

    /// 构造一个 NVBH 描述符（`length` 必须在 `1..=BLOCK_SIZE` 内）。
    pub(crate) fn for_nvbh(
        fs_id: u64,
        inode: u64,
        meta_block: u64,
        length: u32,
        ssd_backing_block: u64,
        recovery_id: u64,
    ) -> Result<Self> {
        if length == 0 || length as usize > BLOCK_SIZE {
            return_errno_with_message!(
                Errno::EINVAL,
                "NVBH length must be within one filesystem block"
            );
        }
        Ok(Self {
            magic: NV_DESCRIPTOR_MAGIC,
            version: NV_DESCRIPTOR_VERSION,
            kind: NvObjectKind::Nvbh as u8,
            state: NvObjectState::NvDirty as u8,
            flags: 0,
            fs_id,
            inode,
            logical_index: meta_block,
            length,
            refs: 0,
            recovery_id,
            ssd_backing_block,
            reserved: 0,
        })
    }

    pub(crate) fn with_allocation_seq(mut self, allocation_seq: u64) -> Result<Self> {
        if allocation_seq == 0 {
            return_errno_with_message!(Errno::EINVAL, "NV allocation sequence is zero");
        }
        self.reserved = allocation_seq;
        Ok(self)
    }

    pub(crate) const fn allocation_seq(&self) -> u64 {
        self.reserved
    }

    pub(crate) const fn has_migration_target(&self) -> bool {
        self.flags & NV_DESCRIPTOR_MIGRATION_TARGET != 0
    }

    pub(crate) const fn migration_credit_released(&self) -> bool {
        self.flags & NV_DESCRIPTOR_MIGRATION_CREDIT_RELEASED != 0
    }

    pub(crate) fn assign_migration_target(&mut self, target: u64) -> Result<()> {
        if target == 0
            || self.kind()? != NvObjectKind::NvPage
            || self.has_migration_target()
            || self.migration_credit_released()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid NVPage migration target");
        }
        self.ssd_backing_block = target;
        self.flags |= NV_DESCRIPTOR_MIGRATION_TARGET;
        match self.state()? {
            // Migration ownership is a DRAM range lease. Persistent authority
            // stays NvDirty until exact SSD publication retires the redirect.
            NvObjectState::NvDirty | NvObjectState::NvWriteback => Ok(()),
            _ => return_errno_with_message!(Errno::EINVAL, "invalid migration target state"),
        }
    }

    pub(crate) fn clear_migration_target(&mut self) -> Result<()> {
        if self.kind()? != NvObjectKind::NvPage || !self.has_migration_target() {
            return_errno_with_message!(Errno::EINVAL, "NVPage has no migration target");
        }
        self.ssd_backing_block = 0;
        self.flags &= !(NV_DESCRIPTOR_MIGRATION_TARGET | NV_DESCRIPTOR_MIGRATION_CREDIT_RELEASED);
        self.transition_to(NvObjectState::NvDirty)
    }

    pub(crate) fn mark_migration_credit_released(&mut self) -> Result<()> {
        if self.kind()? != NvObjectKind::NvPage
            || !self.has_migration_target()
            || self.migration_credit_released()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid migration credit phase");
        }
        self.flags |= NV_DESCRIPTOR_MIGRATION_CREDIT_RELEASED;
        Ok(())
    }

    pub(crate) fn kind(&self) -> Result<NvObjectKind> {
        NvObjectKind::from_u8(self.kind)
    }

    pub(crate) fn state(&self) -> Result<NvObjectState> {
        NvObjectState::from_u8(self.state)
    }

    /// 状态迁移（非法迁移返回 `EINVAL`，`design.md §4.4`）。
    pub(crate) fn transition_to(&mut self, next: NvObjectState) -> Result<()> {
        let current = self.state()?;
        if !current.can_transition_to(next) {
            return_errno_with_message!(Errno::EINVAL, "illegal NV object state transition");
        }
        self.state = next as u8;
        Ok(())
    }

    pub(crate) fn recover_as_dirty(&mut self) -> Result<()> {
        if self.kind()? != NvObjectKind::NvPage || !self.state()?.is_persistable() {
            return_errno_with_message!(Errno::EINVAL, "invalid recovered NVPage state");
        }
        self.state = NvObjectState::NvDirty as u8;
        Ok(())
    }

    pub(crate) fn is_valid(&self) -> bool {
        if self.magic != NV_DESCRIPTOR_MAGIC || self.version != NV_DESCRIPTOR_VERSION {
            return false;
        }
        let Ok(kind) = self.kind() else {
            return false;
        };
        let Ok(state) = self.state() else {
            return false;
        };
        if !state.is_persistable() {
            return false;
        }
        match kind {
            NvObjectKind::NvPage => self.length == PAGE_SIZE as u32,
            NvObjectKind::Nvbh => self.length > 0 && self.length as usize <= BLOCK_SIZE,
        }
    }

    pub(crate) fn encode(&self) -> &[u8] {
        self.as_bytes()
    }

    /// 从字节解码；长度不符或魔数/版本不对即失败（不 panic）。
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        // `Pod::from_bytes` 在长度不符时会 panic，所以先自己查长度。
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "NV descriptor size mismatch");
        }
        let descriptor = Self::from_bytes(bytes);
        if descriptor.magic != NV_DESCRIPTOR_MAGIC {
            return_errno_with_message!(Errno::EINVAL, "not an NV descriptor");
        }
        if descriptor.version != NV_DESCRIPTOR_VERSION {
            return_errno_with_message!(Errno::EINVAL, "unsupported NV descriptor version");
        }
        Ok(descriptor)
    }
}

/// 顺序扫描一段描述符区，返回所有有效描述符。
///
/// 这是 T2.2 要建立的能力：恢复只靠扫描，**不需要**全局物理索引。
/// 槽位里的无效/空洞（`is_valid() == false`）直接跳过。
#[cfg(ktest)]
pub(crate) fn scan_descriptors(bytes: &[u8]) -> Vec<NvDescriptor> {
    let entry_size = size_of::<NvDescriptor>();
    let mut found = Vec::new();
    let mut offset = 0;
    while offset + entry_size <= bytes.len() {
        if let Ok(descriptor) = NvDescriptor::decode(&bytes[offset..offset + entry_size])
            && descriptor.is_valid()
        {
            found.push(descriptor);
        }
        offset += entry_size;
    }
    found
}

#[cfg(any(test, ktest))]
mod recovery_tests {
    use super::*;

    #[cfg_attr(ktest, ostd::prelude::ktest)]
    #[cfg_attr(not(ktest), test)]
    fn recovered_descriptor_layout_and_corruption_checks() {
        assert_eq!(size_of::<NvDescriptor>(), 72);
        assert_eq!(align_of::<NvDescriptor>(), 8);
        let descriptor = NvDescriptor::for_nv_page(1, 2, 3, 4, 5)
            .with_allocation_seq(6)
            .unwrap();
        assert_eq!(
            NvDescriptor::decode(descriptor.encode()).unwrap(),
            descriptor
        );
        assert!(descriptor.is_valid());
        assert!(NvDescriptor::decode(&descriptor.encode()[..71]).is_err());
        let mut corrupt = descriptor;
        corrupt.magic ^= 1;
        assert!(NvDescriptor::decode(corrupt.encode()).is_err());
        corrupt = descriptor;
        corrupt.kind = 255;
        assert!(!corrupt.is_valid());
        corrupt = descriptor;
        corrupt.state = NvObjectState::DramClean as u8;
        assert!(!corrupt.is_valid());
        assert!(!NvObjectState::NvReclaimable.can_transition_to(NvObjectState::DramClean));
    }
}
