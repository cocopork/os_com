// SPDX-License-Identifier: MPL-2.0

//! Persistent RNVPC control-image layout.

use core::mem::offset_of;

use super::descriptor::NvDescriptor;
use crate::prelude::*;

pub(super) const NV_IMAGE_MAGIC: u64 = 0x4e56_504f_4f4c_3031;
/// 布局版本：3 = 自描述 `NvEntry`（T2.2a）。旧版 2 的 `FrameTableEntry`
/// （`namespace`/`home_block`/`slot_bitmap`/`class`）已废弃，不再兼容。
pub(super) const NV_IMAGE_VERSION: u32 = 3;
pub(super) const FRAME_COMMIT_WORD: u64 = 0x434f_4d4d_4954_3031;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct NvImageHeader {
    pub(super) magic: u64,
    pub(super) capacity_pages: u64,
    pub(super) version: u32,
    pub(super) header_size: u32,
    pub(super) entry_size: u32,
    pub(super) flags: u32,
    pub(super) reserved: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct NvEntry {
    /// 自描述描述符：种类、身份 `(fs_id, inode, logical_index)`、长度、引用计数、
    /// 恢复标识，以及**本对象自己的** SSD 物理地址。
    pub(super) descriptor: NvDescriptor,
    /// 两阶段提交字：内容先落盘，再落这个字（否则视为未提交）。
    pub(super) commit_word: u64,
}

const _: () = {
    assert!(size_of::<NvImageHeader>() == 64);
    assert!(align_of::<NvImageHeader>() == 8);
    assert!(size_of::<NvEntry>() == 80);
    assert!(align_of::<NvEntry>() == 8);
    assert!(offset_of!(NvEntry, commit_word) == 72);
};

pub(super) fn control_len(capacity_pages: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            capacity_pages
                .checked_mul(size_of::<NvEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn entry_offset(index: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            index
                .checked_mul(size_of::<NvEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn commit_offset(index: usize) -> Result<usize> {
    entry_offset(index)?
        .checked_add(offset_of!(NvEntry, commit_word))
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}
